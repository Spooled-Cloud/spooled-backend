//! Email login handlers for passwordless authentication
//!
//! This module provides endpoints for:
//! - Starting email login flow (sends 6-digit code)
//! - Verifying email login code (returns JWT tokens)

use axum::{extract::State, Json};
use chrono::{Duration, Utc};
use jsonwebtoken::{encode, EncodingKey, Header};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;
use validator::Validate;

use crate::api::middleware::validation::ValidatedJson;
use crate::api::AppState;
use crate::config::EmailProvider;
use crate::error::{AppError, AppResult};

/// Maximum login code attempts before invalidation
const MAX_CODE_ATTEMPTS: i32 = 5;
/// Login code validity in minutes
const CODE_VALIDITY_MINUTES: i64 = 10;
/// Rate limit: max codes per email per hour
const MAX_CODES_PER_HOUR: i64 = 5;

/// Start email login request
#[derive(Debug, Deserialize, Validate)]
pub struct StartEmailLoginRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
}

/// Start email login response
#[derive(Debug, Serialize)]
pub struct StartEmailLoginResponse {
    pub message: String,
    /// Email sent to (masked for privacy)
    pub email_sent_to: String,
}

/// Start email login - sends a 6-digit code
///
/// POST /api/v1/auth/email/start
pub async fn start(
    State(state): State<AppState>,
    ValidatedJson(request): ValidatedJson<StartEmailLoginRequest>,
) -> AppResult<Json<StartEmailLoginResponse>> {
    let email = request.email.to_lowercase().trim().to_string();

    // Rate limit: check how many codes sent in the last hour
    let recent_codes: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM email_login_codes 
        WHERE email = $1 AND created_at > NOW() - INTERVAL '1 hour'
        "#,
    )
    .bind(&email)
    .fetch_one(state.db.pool())
    .await?;

    if recent_codes.0 >= MAX_CODES_PER_HOUR {
        warn!(email = %email, "Email login rate limit exceeded");
        return Err(AppError::RateLimit(
            "Too many login attempts. Please try again later.".to_string(),
        ));
    }

    // Check if email is associated with an organization
    let org: Option<(String,)> =
        sqlx::query_as("SELECT id FROM organizations WHERE billing_email = $1")
            .bind(&email)
            .fetch_optional(state.db.pool())
            .await?;

    let org_id = org.map(|(id,)| id);

    // Generate a 6-digit code
    let code: String = {
        let mut rng = rand::rng();
        format!("{:06}", rng.random_range(0..1000000))
    };

    let expires_at = Utc::now() + Duration::minutes(CODE_VALIDITY_MINUTES);

    // Store the code
    sqlx::query(
        r#"
        INSERT INTO email_login_codes (email, code, organization_id, expires_at)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(&email)
    .bind(&code)
    .bind(&org_id)
    .bind(expires_at)
    .execute(state.db.pool())
    .await?;

    // Send the email
    send_login_code_email(&state, &email, &code).await?;

    // Mask email for response
    let masked_email = mask_email(&email);

    info!(email = %masked_email, "Login code sent");

    Ok(Json(StartEmailLoginResponse {
        message: "Login code sent to your email".to_string(),
        email_sent_to: masked_email,
    }))
}

/// Verify email login request
#[derive(Debug, Deserialize, Validate)]
pub struct VerifyEmailLoginRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
    #[validate(length(equal = 6, message = "Code must be 6 digits"))]
    pub code: String,
}

/// Verify email login response (same as API key login)
#[derive(Debug, Serialize)]
pub struct VerifyEmailLoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub refresh_expires_in: i64,
}

/// JWT Claims structure (same as auth.rs)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    sub: String,
    api_key_id: String,
    org_id: String,
    iat: i64,
    exp: i64,
    nbf: i64,
    jti: String,
    queues: Vec<String>,
    token_type: String,
    /// Email used for login (for email-based auth)
    email: Option<String>,
}

/// Verify email login - validates code and returns JWT
///
/// POST /api/v1/auth/email/verify
pub async fn verify(
    State(state): State<AppState>,
    ValidatedJson(request): ValidatedJson<VerifyEmailLoginRequest>,
) -> AppResult<Json<VerifyEmailLoginResponse>> {
    let email = request.email.to_lowercase().trim().to_string();
    let code = request.code.trim().to_string();

    // Find valid, unused code
    let login_code: Option<(Uuid, String, Option<String>, i32)> = sqlx::query_as(
        r#"
        SELECT id, code, organization_id, attempts
        FROM email_login_codes
        WHERE email = $1 
          AND expires_at > NOW()
          AND used_at IS NULL
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(&email)
    .fetch_optional(state.db.pool())
    .await?;

    let Some((code_id, stored_code, org_id, attempts)) = login_code else {
        warn!(email = %mask_email(&email), "No valid login code found");
        return Err(AppError::Authentication(
            "Invalid or expired code. Please request a new one.".to_string(),
        ));
    };

    // Check attempts
    if attempts >= MAX_CODE_ATTEMPTS {
        // Invalidate the code
        sqlx::query("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1")
            .bind(code_id)
            .execute(state.db.pool())
            .await?;

        return Err(AppError::Authentication(
            "Too many attempts. Please request a new code.".to_string(),
        ));
    }

    // Increment attempts
    sqlx::query("UPDATE email_login_codes SET attempts = attempts + 1 WHERE id = $1")
        .bind(code_id)
        .execute(state.db.pool())
        .await?;

    // Verify code
    if code != stored_code {
        warn!(email = %mask_email(&email), attempts = attempts + 1, "Invalid login code");
        return Err(AppError::Authentication("Invalid code".to_string()));
    }

    // Mark code as used
    sqlx::query("UPDATE email_login_codes SET used_at = NOW() WHERE id = $1")
        .bind(code_id)
        .execute(state.db.pool())
        .await?;

    // Get or create organization
    let org_id = match org_id {
        Some(id) => id,
        None => {
            // Create a new organization for this email
            create_organization_for_email(&state, &email).await?
        }
    };

    // Get or create an API key for this org (for the token)
    let api_key_id = get_or_create_email_api_key(&state, &org_id, &email).await?;

    // Generate JWT tokens
    let now = Utc::now();
    let access_expiration = state.settings.jwt.expiration_hours as i64;
    let refresh_expiration = access_expiration * 24;

    let access_claims = Claims {
        sub: org_id.clone(),
        api_key_id: api_key_id.clone(),
        org_id: org_id.clone(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(access_expiration)).timestamp(),
        nbf: now.timestamp(),
        jti: Uuid::new_v4().to_string(),
        queues: vec!["*".to_string()], // Email login gets all queues
        token_type: "access".to_string(),
        email: Some(email.clone()),
    };

    let refresh_claims = Claims {
        sub: org_id.clone(),
        api_key_id: api_key_id.clone(),
        org_id: org_id.clone(),
        iat: now.timestamp(),
        exp: (now + Duration::hours(refresh_expiration)).timestamp(),
        nbf: now.timestamp(),
        jti: Uuid::new_v4().to_string(),
        queues: vec!["*".to_string()],
        token_type: "refresh".to_string(),
        email: Some(email.clone()),
    };

    let access_token = encode(
        &Header::default(),
        &access_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode access token");
        AppError::Internal("Failed to generate token".to_string())
    })?;

    let refresh_token = encode(
        &Header::default(),
        &refresh_claims,
        &EncodingKey::from_secret(state.settings.jwt.secret.as_bytes()),
    )
    .map_err(|e| {
        error!(error = %e, "Failed to encode refresh token");
        AppError::Internal("Failed to generate token".to_string())
    })?;

    info!(email = %mask_email(&email), org_id = %org_id, "Email login successful");

    Ok(Json(VerifyEmailLoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: access_expiration * 3600,
        refresh_expires_in: refresh_expiration * 3600,
    }))
}

/// Send login code email
async fn send_login_code_email(state: &AppState, email: &str, code: &str) -> AppResult<()> {
    let subject = "Your Spooled Cloud Login Code";
    let body = format!(
        r#"Hi,

Your login code for Spooled Cloud is: {}

This code will expire in {} minutes.

If you didn't request this code, you can safely ignore this email.

Best,
The Spooled Cloud Team"#,
        code, CODE_VALIDITY_MINUTES
    );

    let html_body = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"></head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <h1 style="color: #10b981; margin: 0;">Spooled Cloud</h1>
    </div>
    <p>Hi,</p>
    <p>Your login code is:</p>
    <div style="text-align: center; margin: 30px 0;">
        <span style="font-size: 32px; font-weight: bold; letter-spacing: 8px; background: #f3f4f6; padding: 16px 32px; border-radius: 8px; font-family: monospace;">{}</span>
    </div>
    <p style="color: #6b7280; font-size: 14px;">This code will expire in {} minutes.</p>
    <p style="color: #6b7280; font-size: 14px;">If you didn't request this code, you can safely ignore this email.</p>
    <hr style="border: none; border-top: 1px solid #e5e7eb; margin: 30px 0;">
    <p style="color: #9ca3af; font-size: 12px; text-align: center;">
        Spooled Cloud - Distributed Job Queue
    </p>
</body>
</html>"#,
        code, CODE_VALIDITY_MINUTES
    );

    match state.settings.email.provider {
        EmailProvider::Console => {
            // Development: log to console
            info!(
                email = %email,
                code = %code,
                "LOGIN CODE EMAIL (console mode - not sent)"
            );
            println!("\n========== LOGIN CODE EMAIL ==========");
            println!("To: {}", email);
            println!("Subject: {}", subject);
            println!("Code: {}", code);
            println!("=======================================\n");
        }
        EmailProvider::Resend => {
            send_via_resend(state, email, subject, &body, &html_body).await?;
        }
        EmailProvider::Sendgrid => {
            send_via_sendgrid(state, email, subject, &body, &html_body).await?;
        }
        EmailProvider::Postmark => {
            send_via_postmark(state, email, subject, &body, &html_body).await?;
        }
        EmailProvider::Smtp => {
            // SMTP would require lettre or similar - for now, fall back to console
            warn!("SMTP email not implemented, falling back to console");
            println!("\n========== LOGIN CODE EMAIL (SMTP fallback) ==========");
            println!("To: {}", email);
            println!("Code: {}", code);
            println!("=======================================================\n");
        }
    }

    Ok(())
}

/// Send email via Resend
async fn send_via_resend(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    let api_key =
        state.settings.email.api_key.as_ref().ok_or_else(|| {
            AppError::Internal("EMAIL_API_KEY not configured for Resend".to_string())
        })?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "from": format!("{} <{}>", state.settings.email.from_name, state.settings.email.from_address),
            "to": [to],
            "subject": subject,
            "text": text,
            "html": html
        }))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email: {}", e)))?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        error!(error = %error, "Resend email failed");
        return Err(AppError::Internal("Failed to send email".to_string()));
    }

    Ok(())
}

/// Send email via SendGrid
async fn send_via_sendgrid(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    let api_key = state.settings.email.api_key.as_ref().ok_or_else(|| {
        AppError::Internal("EMAIL_API_KEY not configured for SendGrid".to_string())
    })?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.sendgrid.com/v3/mail/send")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "personalizations": [{
                "to": [{"email": to}]
            }],
            "from": {
                "email": state.settings.email.from_address,
                "name": state.settings.email.from_name
            },
            "subject": subject,
            "content": [
                {"type": "text/plain", "value": text},
                {"type": "text/html", "value": html}
            ]
        }))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email: {}", e)))?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        error!(error = %error, "SendGrid email failed");
        return Err(AppError::Internal("Failed to send email".to_string()));
    }

    Ok(())
}

/// Send email via Postmark
async fn send_via_postmark(
    state: &AppState,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> AppResult<()> {
    let api_key = state.settings.email.api_key.as_ref().ok_or_else(|| {
        AppError::Internal("EMAIL_API_KEY not configured for Postmark".to_string())
    })?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.postmarkapp.com/email")
        .header("X-Postmark-Server-Token", api_key)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "From": format!("{} <{}>", state.settings.email.from_name, state.settings.email.from_address),
            "To": to,
            "Subject": subject,
            "TextBody": text,
            "HtmlBody": html
        }))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to send email: {}", e)))?;

    if !response.status().is_success() {
        let error = response.text().await.unwrap_or_default();
        error!(error = %error, "Postmark email failed");
        return Err(AppError::Internal("Failed to send email".to_string()));
    }

    Ok(())
}

/// Create organization for email
async fn create_organization_for_email(state: &AppState, email: &str) -> AppResult<String> {
    let org_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate slug from email
    let slug = email
        .split('@')
        .next()
        .unwrap_or("user")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(20)
        .collect::<String>()
        .to_lowercase();

    let slug = if slug.len() < 3 {
        format!("{}-{}", slug, &org_id[..6])
    } else {
        // Add random suffix to ensure uniqueness
        format!("{}-{}", slug, &org_id[..6])
    };

    let name = email.split('@').next().unwrap_or("User").to_string();

    sqlx::query(
        r#"
        INSERT INTO organizations (id, name, slug, plan_tier, billing_email, settings, created_at, updated_at)
        VALUES ($1, $2, $3, 'free', $4, '{}', $5, $5)
        "#,
    )
    .bind(&org_id)
    .bind(&name)
    .bind(&slug)
    .bind(email)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    info!(org_id = %org_id, email = %mask_email(email), "Created organization for email login");

    Ok(org_id)
}

/// Get or create API key for email login
async fn get_or_create_email_api_key(
    state: &AppState,
    org_id: &str,
    email: &str,
) -> AppResult<String> {
    // Check for existing email-based API key
    let existing: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT id FROM api_keys 
        WHERE organization_id = $1 
          AND name = 'Email Login'
          AND is_active = TRUE
        LIMIT 1
        "#,
    )
    .bind(org_id)
    .fetch_optional(state.db.pool())
    .await?;

    if let Some((id,)) = existing {
        return Ok(id);
    }

    // Create new API key
    let key_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    // Generate a random key (we won't expose it, just for internal use)
    let raw_key = format!("sk_email_{}", Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = bcrypt::hash(&raw_key, 10)
        .map_err(|e| AppError::Internal(format!("Failed to hash key: {}", e)))?;
    let key_prefix = raw_key.chars().take(8).collect::<String>();

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, organization_id, name, key_hash, key_prefix, queues, is_active, created_at)
        VALUES ($1, $2, 'Email Login', $3, $4, ARRAY['*'], TRUE, $5)
        "#,
    )
    .bind(&key_id)
    .bind(org_id)
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(now)
    .execute(state.db.pool())
    .await?;

    info!(
        org_id = %org_id,
        email = %mask_email(email),
        "Created API key for email login"
    );

    Ok(key_id)
}

/// Mask email for privacy in logs and responses
fn mask_email(email: &str) -> String {
    let parts: Vec<&str> = email.split('@').collect();
    if parts.len() != 2 {
        return "***@***".to_string();
    }

    let local = parts[0];
    let domain = parts[1];

    let masked_local = if local.len() <= 2 {
        "*".repeat(local.len())
    } else {
        format!("{}***{}", &local[..1], &local[local.len() - 1..])
    };

    format!("{}@{}", masked_local, domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_email() {
        assert_eq!(mask_email("john@example.com"), "j***n@example.com");
        assert_eq!(mask_email("ab@example.com"), "**@example.com");
        assert_eq!(mask_email("a@test.com"), "*@test.com");
        assert_eq!(mask_email("test.user@company.co"), "t***r@company.co");
    }

    #[test]
    fn test_mask_email_invalid() {
        assert_eq!(mask_email("invalid"), "***@***");
        assert_eq!(mask_email(""), "***@***");
    }
}
