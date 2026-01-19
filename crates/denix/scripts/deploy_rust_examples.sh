#!/bin/bash

# 部署 Rust 平台的示例应用

DENIX_URL="${DENIX_URL:-http://localhost:8000}"

echo "🚀 Deploying example apps to Denix (Rust)..."
echo "📡 Target: $DENIX_URL"
echo ""

# 检查服务器是否运行
if ! curl -s "$DENIX_URL/health" > /dev/null; then
    echo "❌ Error: Denix server is not running!"
    echo "   Please start it first: cargo run"
    exit 1
fi

# 部署函数
deploy_app() {
    local id=$1
    local file=$2
    local description=$3

    echo "📦 Deploying $id..."
    echo "   $description"

    # 读取应用代码并转义
    local code=$(cat "$file" | jq -Rs .)

    # 发送部署请求
    local response=$(curl -s -w "\n%{http_code}" -X POST "$DENIX_URL/api/apps" \
        -H "Content-Type: application/json" \
        -d "{\"id\":\"$id\",\"code\":$code}")

    local status_code=$(echo "$response" | tail -n 1)
    local body=$(echo "$response" | head -n -1)

    if [ "$status_code" = "201" ]; then
        echo "   ✅ Deployed successfully"
        echo "   🌐 Access at: $DENIX_URL/apps/$id"
    else
        echo "   ❌ Failed to deploy: $body"
    fi
    echo ""
}

# 部署所有应用
deploy_app "hello" "apps/hello.ts" "Simple Hello World app"
deploy_app "counter" "apps/counter.ts" "Counter app using KV storage"
deploy_app "echo" "apps/echo.ts" "Echo app that returns request info"
deploy_app "todos" "apps/todo.ts" "Simple TODO app with CRUD operations"

echo "✅ All apps deployed!"
echo ""
echo "📚 Try these commands:"
echo "   curl $DENIX_URL/apps/hello"
echo "   curl $DENIX_URL/apps/counter/increment"
echo "   curl $DENIX_URL/apps/echo"
echo "   curl $DENIX_URL/apps/todos/todos"
