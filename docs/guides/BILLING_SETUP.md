# Stripe Billing Setup Guide

Complete guide to setting up Stripe payments for Spooled Cloud.

## 📋 Overview

Spooled uses Stripe for subscription billing with the following features:

| Feature | Status | Description |
|---------|--------|-------------|
| **Checkout** | ✅ Implemented | Via Stripe Payment Links |
| **Plan Switching** | ✅ Implemented | Automatic via webhooks |
| **Auto-Downgrade** | ✅ Implemented | On subscription cancel |
| **Past Due Handling** | ✅ Implemented | On payment failure |
| **Billing Portal** | ✅ Implemented | Self-service management |
| **Webhook Security** | ✅ Implemented | HMAC-SHA256 verification |

## 🏗️ Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│   Frontend      │────▶│  Stripe Payment  │────▶│   Stripe        │
│   /pricing      │     │  Links           │     │   Checkout      │
└─────────────────┘     └──────────────────┘     └────────┬────────┘
                                                          │
                                                          ▼
┌─────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│   Dashboard     │◀────│   Backend API    │◀────│   Stripe        │
│   /billing      │     │   /billing/*     │     │   Webhooks      │
└─────────────────┘     └──────────────────┘     └─────────────────┘
```

## 🔧 Step 1: Create Stripe Products & Prices

### 1.1 Log in to Stripe Dashboard

Go to [https://dashboard.stripe.com](https://dashboard.stripe.com)

### 1.2 Create Products

Navigate to **Products** → **Add product**

Create two products:

#### Starter Plan
```
Name: Spooled Starter
Description: For growing teams and apps
Pricing: $19/month (recurring)
```

#### Pro Plan
```
Name: Spooled Pro
Description: For production workloads
Pricing: $49/month (recurring)
```

### 1.3 Copy Price IDs

After creating each product, copy the **Price ID** (starts with `price_`):

```
STRIPE_STARTER_PRICE_ID=price_1Abc123...
STRIPE_PRO_PRICE_ID=price_1Xyz789...
```

## 🔧 Step 2: Create Payment Links

Payment Links allow customers to subscribe without server-side checkout session creation.

### 2.1 Create Payment Links

Go to **Payment Links** → **New**

For each plan:

1. Select the product/price
2. **Settings:**
   - ✅ Allow promotion codes (optional)
   - ✅ Collect billing address
   - ✅ Collect tax automatically (optional)
3. **After payment:**
   - Redirect to: `https://your-frontend.com/billing/success`
4. **Advanced:**
   - Enable `client_reference_id` parameter
   
### 2.2 Copy Payment Link URLs

Get the shareable links (e.g., `https://buy.stripe.com/xxx`):

```
PUBLIC_STRIPE_LINK_STARTER=https://buy.stripe.com/starter_xxx?client_reference_id=
PUBLIC_STRIPE_LINK_PRO=https://buy.stripe.com/pro_xxx?client_reference_id=
```

**Important:** Include `?client_reference_id=` at the end. The frontend will append the `org_id`.

## 🔧 Step 3: Configure Webhook Endpoint

### 3.1 Create Webhook Endpoint

Go to **Developers** → **Webhooks** → **Add endpoint**

```
Endpoint URL: https://api.your-domain.com/api/v1/billing/webhook
Description: Spooled billing events
```

### 3.2 Select Events

Select these events:

```
✅ checkout.session.completed
✅ customer.subscription.created
✅ customer.subscription.updated
✅ customer.subscription.deleted
✅ invoice.paid
✅ invoice.payment_failed
```

### 3.3 Copy Webhook Signing Secret

After creating, click **Reveal** to get the signing secret:

```
STRIPE_BILLING_WEBHOOK_SECRET=whsec_xxxxx
```

## 🔧 Step 4: Configure Billing Portal (Optional)

The Stripe Billing Portal allows customers to:
- Update payment method
- View invoices
- Cancel/reactivate subscription

### 4.1 Configure Portal

Go to **Settings** → **Billing** → **Customer portal**

Configure:
- ✅ Allow customers to update payment methods
- ✅ Allow customers to view invoice history
- ✅ Allow customers to cancel subscriptions
- ✅ Allow plan switching (if you want)

### 4.2 Save Configuration

If you create a custom configuration, copy the Configuration ID:

```
STRIPE_BILLING_PORTAL_CONFIG_ID=bpc_xxxxx
```

(This is optional - the default configuration works fine.)

## 🔧 Step 5: Set Environment Variables

### Backend Environment Variables

```bash
# Required for billing
STRIPE_SECRET_KEY=sk_live_xxxxx           # Or sk_test_xxxxx for testing
STRIPE_BILLING_WEBHOOK_SECRET=whsec_xxxxx # Webhook signing secret
STRIPE_STARTER_PRICE_ID=price_xxxxx       # Starter plan price ID
STRIPE_PRO_PRICE_ID=price_xxxxx           # Pro plan price ID

# Optional
STRIPE_BILLING_PORTAL_CONFIG_ID=bpc_xxxxx # Custom portal configuration
```

### Frontend Environment Variables

```bash
# Stripe Payment Links (include ?client_reference_id= at end)
PUBLIC_STRIPE_LINK_STARTER=https://buy.stripe.com/xxx?client_reference_id=
PUBLIC_STRIPE_LINK_PRO=https://buy.stripe.com/xxx?client_reference_id=

# Pricing display
PUBLIC_PRICE_CURRENCY=USD
PUBLIC_PRICE_STARTER_USD=19
PUBLIC_PRICE_PRO_USD=49
```

### Dashboard Environment Variables

```bash
# API URL (for billing status/portal calls)
PUBLIC_API_URL=https://api.your-domain.com
```

## 🔄 How It Works

### Checkout Flow

```
1. User clicks "Upgrade to Pro" on /pricing
2. Frontend appends org_id: https://buy.stripe.com/pro?client_reference_id=org_xxx
3. User completes Stripe Checkout
4. Stripe sends checkout.session.completed webhook
5. Backend links stripe_customer_id to organization
6. Stripe sends customer.subscription.created webhook
7. Backend updates plan_tier to "pro"
8. User is redirected to /billing/success
```

### Subscription Lifecycle

```
checkout.session.completed
  └── Link customer to organization
  
customer.subscription.created
customer.subscription.updated
  └── Update: subscription_id, status, period_end, plan_tier
  
invoice.paid
  └── Set status = "active"
  
invoice.payment_failed
  └── Set status = "past_due"
  
customer.subscription.deleted
  └── Downgrade to "free" tier
```

### Plan Detection

The backend matches the subscription's `price_id` against configured environment variables:

```rust
if price_id == STRIPE_STARTER_PRICE_ID {
    plan_tier = "starter"
} else if price_id == STRIPE_PRO_PRICE_ID {
    plan_tier = "pro"
}
```

## 🧪 Testing

### Test Mode

Use Stripe test mode for development:

1. Use `sk_test_` keys instead of `sk_live_`
2. Use test Payment Links
3. Use [Stripe test cards](https://stripe.com/docs/testing):
   - Success: `4242 4242 4242 4242`
   - Decline: `4000 0000 0000 0002`
   - Requires auth: `4000 0025 0000 3155`

### Test Webhooks Locally

Use Stripe CLI to forward webhooks:

```bash
# Install Stripe CLI
brew install stripe/stripe-cli/stripe

# Login
stripe login

# Forward webhooks to local server
stripe listen --forward-to localhost:8080/api/v1/billing/webhook

# Copy the webhook signing secret it provides
# Use this for STRIPE_BILLING_WEBHOOK_SECRET locally
```

### Trigger Test Events

```bash
# Trigger a subscription created event
stripe trigger customer.subscription.created

# Trigger payment failure
stripe trigger invoice.payment_failed
```

## 📊 Database Schema

The `organizations` table stores billing data:

```sql
CREATE TABLE organizations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT UNIQUE NOT NULL,
    plan_tier TEXT DEFAULT 'free',        -- free, starter, pro, enterprise
    billing_email TEXT,
    stripe_customer_id TEXT,              -- cus_xxxxx
    stripe_subscription_id TEXT,          -- sub_xxxxx
    stripe_subscription_status TEXT,      -- active, past_due, canceled, etc.
    stripe_current_period_end TIMESTAMPTZ,-- When subscription renews/ends
    stripe_cancel_at_period_end BOOLEAN,  -- If scheduled for cancellation
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);
```

## 🔒 Security

### Webhook Verification

All Stripe webhooks are verified using HMAC-SHA256:

```rust
// Backend verifies:
1. Stripe-Signature header present
2. Timestamp within 5 minutes
3. HMAC signature matches using STRIPE_BILLING_WEBHOOK_SECRET
```

### Best Practices

1. **Never expose `STRIPE_SECRET_KEY`** in frontend code
2. **Always verify webhook signatures** (already implemented)
3. **Use HTTPS** for all webhook endpoints
4. **Monitor webhook failures** in Stripe dashboard
5. **Idempotency:** Stripe may send webhooks multiple times; our handlers are idempotent

## 🚨 Troubleshooting

### Webhooks Not Received

1. Check endpoint URL is correct and publicly accessible
2. Check Stripe dashboard → Webhooks → Recent events for failures
3. Verify `STRIPE_BILLING_WEBHOOK_SECRET` matches the endpoint

### Plan Not Updating

1. Verify webhook events are selected
2. Check `STRIPE_STARTER_PRICE_ID` and `STRIPE_PRO_PRICE_ID` match Stripe
3. Check backend logs for webhook processing errors

### Customer Not Linked

1. Ensure Payment Link includes `?client_reference_id=` parameter
2. Frontend must append `org_id` to the link
3. Check `checkout.session.completed` webhook received

### Billing Portal Error

1. Verify `STRIPE_SECRET_KEY` is set
2. Check customer has `stripe_customer_id` in database
3. Customer must have completed checkout first

## 📱 Frontend Pages

| Page | Purpose |
|------|---------|
| `/pricing` | Plan comparison, upgrade CTAs |
| `/account/billing` | Current plan, billing portal access |
| `/billing/success` | Post-checkout success page |
| `/contact` | Enterprise plan inquiries |

## 🎯 Quick Checklist

```bash
# 1. Stripe Products
[ ] Created Starter product with monthly price
[ ] Created Pro product with monthly price
[ ] Copied price IDs

# 2. Payment Links
[ ] Created Starter payment link with client_reference_id
[ ] Created Pro payment link with client_reference_id
[ ] Set redirect to /billing/success

# 3. Webhooks
[ ] Created webhook endpoint
[ ] Selected all required events
[ ] Copied signing secret

# 4. Backend Environment
[ ] STRIPE_SECRET_KEY set
[ ] STRIPE_BILLING_WEBHOOK_SECRET set
[ ] STRIPE_STARTER_PRICE_ID set
[ ] STRIPE_PRO_PRICE_ID set

# 5. Frontend Environment
[ ] PUBLIC_STRIPE_LINK_STARTER set
[ ] PUBLIC_STRIPE_LINK_PRO set
[ ] PUBLIC_PRICE_STARTER_USD set
[ ] PUBLIC_PRICE_PRO_USD set

# 6. Testing
[ ] Test checkout in Stripe test mode
[ ] Verify webhook received
[ ] Verify plan updated in database
[ ] Test billing portal access
[ ] Test cancellation flow
```

## 📞 Support

For Enterprise plans and custom billing arrangements, customers should contact sales via `/contact` page.

---

**The billing system is fully implemented and ready for production!**
