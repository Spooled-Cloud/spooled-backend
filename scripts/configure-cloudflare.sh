#!/bin/bash
# Configure Cloudflare to allow high API usage without challenges
#
# Required environment variables:
#   CLOUDFLARE_API_TOKEN - API token with Zone:Edit permissions
#   CLOUDFLARE_ZONE_ID   - Zone ID for spooled.cloud
#
# Get these from: https://dash.cloudflare.com/profile/api-tokens
# Zone ID is in: Cloudflare Dashboard → spooled.cloud → Overview (right sidebar)

set -e

if [ -z "$CLOUDFLARE_API_TOKEN" ] || [ -z "$CLOUDFLARE_ZONE_ID" ]; then
    echo "Error: CLOUDFLARE_API_TOKEN and CLOUDFLARE_ZONE_ID must be set"
    echo ""
    echo "Get API Token: https://dash.cloudflare.com/profile/api-tokens"
    echo "  - Create token with 'Zone Settings: Edit' and 'Zone WAF: Edit' permissions"
    echo ""
    echo "Get Zone ID: Cloudflare Dashboard → spooled.cloud → Overview → API section (right sidebar)"
    exit 1
fi

API_BASE="https://api.cloudflare.com/client/v4"
ZONE_ID="$CLOUDFLARE_ZONE_ID"
AUTH_HEADER="Authorization: Bearer $CLOUDFLARE_API_TOKEN"

echo "=== Configuring Cloudflare for spooled.cloud ==="

# 1. Set Security Level to "low" (allows most traffic without challenge)
echo ""
echo "1. Setting Security Level to 'low'..."
curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/security_level" \
    -H "$AUTH_HEADER" \
    -H "Content-Type: application/json" \
    --data '{"value":"low"}' | jq -r '.success'

# 2. Disable Browser Integrity Check (it blocks automated API requests)
echo "2. Disabling Browser Integrity Check..."
curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/browser_check" \
    -H "$AUTH_HEADER" \
    -H "Content-Type: application/json" \
    --data '{"value":"off"}' | jq -r '.success'

# 3. Set Bot Fight Mode to minimal (don't challenge API bots)
echo "3. Configuring Bot Fight Mode..."
# Note: Super Bot Fight Mode requires enterprise plan, this sets standard bot mode
curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/bot_management" \
    -H "$AUTH_HEADER" \
    -H "Content-Type: application/json" \
    --data '{"value":{"fight_mode":false}}' 2>/dev/null | jq -r '.success' || echo "skipped (requires paid plan)"

# 4. Create/Update WAF rule to skip security for API subdomain
echo "4. Creating WAF bypass rule for api.spooled.cloud..."

# First, check if rule exists
EXISTING_RULE=$(curl -s "$API_BASE/zones/$ZONE_ID/firewall/rules" \
    -H "$AUTH_HEADER" \
    -H "Content-Type: application/json" | jq -r '.result[] | select(.description=="API Bypass Security") | .id')

FILTER_EXPRESSION='(http.host eq "api.spooled.cloud")'

if [ -n "$EXISTING_RULE" ]; then
    echo "   Rule exists, updating..."
    # Update existing rule
    curl -s -X PUT "$API_BASE/zones/$ZONE_ID/firewall/rules/$EXISTING_RULE" \
        -H "$AUTH_HEADER" \
        -H "Content-Type: application/json" \
        --data "{
            \"description\": \"API Bypass Security\",
            \"action\": \"allow\",
            \"filter\": {
                \"expression\": \"$FILTER_EXPRESSION\",
                \"paused\": false
            },
            \"paused\": false
        }" | jq -r '.success'
else
    echo "   Creating new rule..."
    # Create filter first
    FILTER_ID=$(curl -s -X POST "$API_BASE/zones/$ZONE_ID/filters" \
        -H "$AUTH_HEADER" \
        -H "Content-Type: application/json" \
        --data "[{\"expression\": \"$FILTER_EXPRESSION\"}]" | jq -r '.result[0].id')
    
    if [ -n "$FILTER_ID" ] && [ "$FILTER_ID" != "null" ]; then
        # Create rule with filter
        curl -s -X POST "$API_BASE/zones/$ZONE_ID/firewall/rules" \
            -H "$AUTH_HEADER" \
            -H "Content-Type: application/json" \
            --data "[{
                \"description\": \"API Bypass Security\",
                \"action\": \"allow\",
                \"filter\": {\"id\": \"$FILTER_ID\"},
                \"paused\": false
            }]" | jq -r '.success'
    else
        echo "   Failed to create filter"
    fi
fi

# 5. Increase Rate Limiting thresholds
echo "5. Checking rate limiting rules..."
curl -s "$API_BASE/zones/$ZONE_ID/rate_limits" \
    -H "$AUTH_HEADER" \
    -H "Content-Type: application/json" | jq -r '.result | length | "   Found \(.) rate limit rules"'

# 6. Check Under Attack Mode (should be off)
echo "6. Checking Under Attack Mode..."
UAM_STATUS=$(curl -s "$API_BASE/zones/$ZONE_ID/settings/security_level" \
    -H "$AUTH_HEADER" \
    -H "Content-Type: application/json" | jq -r '.result.value')
if [ "$UAM_STATUS" = "under_attack" ]; then
    echo "   WARNING: Under Attack Mode is ON - this will challenge all visitors!"
    echo "   Disabling..."
    curl -s -X PATCH "$API_BASE/zones/$ZONE_ID/settings/security_level" \
        -H "$AUTH_HEADER" \
        -H "Content-Type: application/json" \
        --data '{"value":"low"}' | jq -r '.success'
else
    echo "   Under Attack Mode is OFF (good)"
fi

echo ""
echo "=== Configuration Complete ==="
echo ""
echo "Current settings:"
echo "  - Security Level: low"
echo "  - Browser Integrity Check: disabled"  
echo "  - WAF Rule: api.spooled.cloud bypasses security"
echo ""
echo "If you still see blocking, check the Cloudflare Dashboard for:"
echo "  1. Security → Events - see what's blocking requests"
echo "  2. Security → WAF → Rate limiting rules - may need to increase limits"
echo "  3. Security → Bots - configure bot protection settings"

