#!/bin/bash
# Configure Cloudflare to allow high API usage without challenges
#
# For API Token (recommended):
#   CLOUDFLARE_API_TOKEN - API token with Zone:Edit permissions
#   CLOUDFLARE_ZONE_ID   - Zone ID for spooled.cloud
#
# For Global API Key (legacy):
#   CLOUDFLARE_API_KEY   - Global API Key
#   CLOUDFLARE_EMAIL     - Account email
#   CLOUDFLARE_ZONE_ID   - Zone ID

set -e

API_BASE="https://api.cloudflare.com/client/v4"
ZONE_ID="$CLOUDFLARE_ZONE_ID"

# Determine auth method
if [ -n "$CLOUDFLARE_API_TOKEN" ]; then
    # Clean the token (remove any whitespace/newlines)
    TOKEN=$(echo "$CLOUDFLARE_API_TOKEN" | tr -d '[:space:]')
    AUTH_HEADERS=(-H "Authorization: Bearer $TOKEN")
    AUTH_TYPE="API Token"
elif [ -n "$CLOUDFLARE_API_KEY" ] && [ -n "$CLOUDFLARE_EMAIL" ]; then
    KEY=$(echo "$CLOUDFLARE_API_KEY" | tr -d '[:space:]')
    EMAIL=$(echo "$CLOUDFLARE_EMAIL" | tr -d '[:space:]')
    AUTH_HEADERS=(-H "X-Auth-Key: $KEY" -H "X-Auth-Email: $EMAIL")
    AUTH_TYPE="Global API Key"
else
    echo "Error: Authentication not configured"
    echo ""
    echo "Option 1 - API Token (recommended):"
    echo "  export CLOUDFLARE_API_TOKEN='your-token'"
    echo "  export CLOUDFLARE_ZONE_ID='your-zone-id'"
    echo ""
    echo "Option 2 - Global API Key:"
    echo "  export CLOUDFLARE_API_KEY='your-global-api-key'"
    echo "  export CLOUDFLARE_EMAIL='your-email@example.com'"
    echo "  export CLOUDFLARE_ZONE_ID='your-zone-id'"
    echo ""
    echo "Get credentials: https://dash.cloudflare.com/profile/api-tokens"
    exit 1
fi

if [ -z "$ZONE_ID" ]; then
    echo "Error: CLOUDFLARE_ZONE_ID not set"
    echo "Get Zone ID: Dashboard → spooled.cloud → Overview → right sidebar"
    exit 1
fi

echo "=== Configuring Cloudflare for spooled.cloud ==="
echo "Auth Type: $AUTH_TYPE"
echo "Zone ID: $ZONE_ID"
echo ""

# Verify credentials
echo "Verifying credentials..."
if [ "$AUTH_TYPE" = "API Token" ]; then
    VERIFY=$(curl -s "$API_BASE/user/tokens/verify" "${AUTH_HEADERS[@]}")
else
    VERIFY=$(curl -s "$API_BASE/user" "${AUTH_HEADERS[@]}")
fi

SUCCESS=$(echo "$VERIFY" | jq -r '.success')
if [ "$SUCCESS" != "true" ]; then
    echo "ERROR: Authentication failed!"
    echo "$VERIFY" | jq '.'
    exit 1
fi
echo "✓ Credentials verified"
echo ""

# Verify zone access
echo "Verifying zone access..."
ZONE_INFO=$(curl -s "$API_BASE/zones/$ZONE_ID" "${AUTH_HEADERS[@]}")
if [ "$(echo "$ZONE_INFO" | jq -r '.success')" != "true" ]; then
    echo "ERROR: Cannot access zone!"
    echo "$ZONE_INFO" | jq '.'
    exit 1
fi
ZONE_NAME=$(echo "$ZONE_INFO" | jq -r '.result.name')
echo "✓ Zone: $ZONE_NAME"
echo ""

# 1. Set Security Level to "essentially_off" (lowest)
echo "1. Setting Security Level to 'essentially_off'..."
RESULT=$(curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/security_level" \
    "${AUTH_HEADERS[@]}" \
    -H "Content-Type: application/json" \
    --data '{"value":"essentially_off"}')
if [ "$(echo "$RESULT" | jq -r '.success')" = "true" ]; then
    echo "   ✓ Security level set to: $(echo "$RESULT" | jq -r '.result.value')"
else
    echo "   ✗ Failed: $(echo "$RESULT" | jq -r '.errors[0].message')"
fi
echo ""

# 2. Disable Browser Integrity Check
echo "2. Disabling Browser Integrity Check..."
RESULT=$(curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/browser_check" \
    "${AUTH_HEADERS[@]}" \
    -H "Content-Type: application/json" \
    --data '{"value":"off"}')
if [ "$(echo "$RESULT" | jq -r '.success')" = "true" ]; then
    echo "   ✓ Browser check: $(echo "$RESULT" | jq -r '.result.value')"
else
    echo "   ✗ Failed: $(echo "$RESULT" | jq -r '.errors[0].message')"
fi
echo ""

# 3. Disable Challenge Passage (how long challenges are remembered)
echo "3. Setting challenge passage to maximum (1 year)..."
RESULT=$(curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/challenge_ttl" \
    "${AUTH_HEADERS[@]}" \
    -H "Content-Type: application/json" \
    --data '{"value":31536000}')
if [ "$(echo "$RESULT" | jq -r '.success')" = "true" ]; then
    echo "   ✓ Challenge TTL: $(echo "$RESULT" | jq -r '.result.value') seconds"
else
    echo "   ✗ Failed: $(echo "$RESULT" | jq -r '.errors[0].message')"
fi
echo ""

# 4. Check and disable any IP-based blocks
echo "4. Checking IP Access Rules..."
IP_RULES=$(curl -s "$API_BASE/zones/$ZONE_ID/firewall/access_rules/rules" "${AUTH_HEADERS[@]}")
RULE_COUNT=$(echo "$IP_RULES" | jq -r '.result | length')
echo "   Found $RULE_COUNT IP access rules"
if [ "$RULE_COUNT" -gt 0 ]; then
    echo "$IP_RULES" | jq -r '.result[] | "   - \(.configuration.value): \(.mode)"'
fi
echo ""

# 5. Check rate limiting rules
echo "5. Checking Rate Limiting Rules..."
RATE_RULES=$(curl -s "$API_BASE/zones/$ZONE_ID/rate_limits" "${AUTH_HEADERS[@]}")
RATE_COUNT=$(echo "$RATE_RULES" | jq -r '.result | length')
echo "   Found $RATE_COUNT rate limiting rules"
if [ "$RATE_COUNT" -gt 0 ]; then
    echo "$RATE_RULES" | jq -r '.result[] | "   - \(.description // "No description"): \(.threshold) req/\(.period)s"'
fi
echo ""

# 6. Check Security Events (what's been blocked recently)
echo "6. Recent Security Events..."
# Get events from last hour
SINCE=$(date -u -v-1H +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || date -u -d '1 hour ago' +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || echo "")
if [ -n "$SINCE" ]; then
    EVENTS=$(curl -s "$API_BASE/zones/$ZONE_ID/security/events?since=$SINCE&limit=5" "${AUTH_HEADERS[@]}")
    if [ "$(echo "$EVENTS" | jq -r '.success')" = "true" ]; then
        EVENT_COUNT=$(echo "$EVENTS" | jq -r '.result | length')
        echo "   Last hour: $EVENT_COUNT events"
        if [ "$EVENT_COUNT" -gt 0 ]; then
            echo "$EVENTS" | jq -r '.result[:5][] | "   - \(.action): \(.clientIP) (\(.ruleId // "unknown rule"))"'
        fi
    else
        echo "   Could not fetch events (may require higher plan)"
    fi
else
    echo "   Skipped (date command issue)"
fi
echo ""

# 7. Summary
echo "=== Configuration Complete ==="
echo ""
echo "Current Settings:"
SETTINGS=$(curl -s "$API_BASE/zones/$ZONE_ID/settings" "${AUTH_HEADERS[@]}")
echo "  Security Level: $(echo "$SETTINGS" | jq -r '.result[] | select(.id=="security_level") | .value')"
echo "  Browser Check: $(echo "$SETTINGS" | jq -r '.result[] | select(.id=="browser_check") | .value')"
echo "  Challenge TTL: $(echo "$SETTINGS" | jq -r '.result[] | select(.id=="challenge_ttl") | .value')s"
echo ""
echo "If you're STILL getting blocked, it's likely:"
echo "  1. Bot Fight Mode - Go to Security → Bots → Configure → Set all to 'Allow'"
echo "  2. Under Attack Mode - Make sure it's OFF in Security → Settings"
echo "  3. Custom WAF Rules - Check Security → WAF → Custom rules"
echo ""
echo "Direct link: https://dash.cloudflare.com/?zone=$ZONE_ID"
