#!/bin/bash

# Denix 平台测试脚本

DENIX_URL="${DENIX_URL:-http://localhost:8000}"

echo "🧪 Testing Denix Platform"
echo "=========================="
echo ""

# 颜色定义
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# 测试函数
test_endpoint() {
  local name="$1"
  local method="$2"
  local endpoint="$3"
  local data="$4"
  local expected_status="$5"

  echo -n "Testing: $name... "

  if [ -z "$data" ]; then
    response=$(curl -s -w "\n%{http_code}" -X "$method" "$DENIX_URL$endpoint")
  else
    response=$(curl -s -w "\n%{http_code}" -X "$method" "$DENIX_URL$endpoint" \
      -H "Content-Type: application/json" \
      -d "$data")
  fi

  status_code=$(echo "$response" | tail -n 1)
  body=$(echo "$response" | head -n -1)

  if [ "$status_code" = "$expected_status" ]; then
    echo -e "${GREEN}✓ PASS${NC} (HTTP $status_code)"
  else
    echo -e "${RED}✗ FAIL${NC} (Expected $expected_status, got $status_code)"
    echo "Response: $body"
  fi
}

# 1. 测试健康检查
echo "1️⃣  Health Check"
test_endpoint "Health endpoint" "GET" "/health" "" "200"
echo ""

# 2. 测试应用部署
echo "2️⃣  App Deployment"
test_endpoint "Deploy test app" "POST" "/api/apps" \
  '{"id":"test","code":"export default { fetch() { return new Response(\"test\") } }"}' \
  "201"
echo ""

# 3. 测试应用列表
echo "3️⃣  App Management"
test_endpoint "List apps" "GET" "/api/apps" "" "200"
test_endpoint "Get app info" "GET" "/api/apps/test" "" "200"
echo ""

# 4. 测试应用执行
echo "4️⃣  App Execution"
test_endpoint "Execute app" "GET" "/apps/test" "" "200"
echo ""

# 5. 测试应用更新
echo "5️⃣  App Update"
test_endpoint "Update app" "PUT" "/api/apps/test" \
  '{"code":"export default { fetch() { return new Response(\"updated\") } }"}' \
  "200"
test_endpoint "Execute updated app" "GET" "/apps/test" "" "200"
echo ""

# 6. 测试应用删除
echo "6️⃣  App Deletion"
test_endpoint "Delete app" "DELETE" "/api/apps/test" "" "200"
test_endpoint "Access deleted app" "GET" "/apps/test" "" "404"
echo ""

echo "=========================="
echo "✅ Tests completed!"
