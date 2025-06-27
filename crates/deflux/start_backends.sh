#!/bin/bash

# 启动多个测试后端服务器的脚本

echo "正在启动测试后端服务器..."

# 定义后端服务器端口
PORTS=(8001 8002 8003)

# 存储后台进程 PID
PIDS=()

# 启动后端服务器
for port in "${PORTS[@]}"; do
    echo "启动后端服务器，端口: $port"
    python3 test_backend.py $port &
    PIDS+=($!)
    sleep 1
done

echo "所有后端服务器已启动"
echo "后端服务器进程 PID: ${PIDS[*]}"
echo ""
echo "测试 URL:"
echo "  http://127.0.0.1:8001"
echo "  http://127.0.0.1:8002"
echo "  http://127.0.0.1:8003"
echo ""
echo "负载均衡器测试 URL:"
echo "  http://127.0.0.1:6188"
echo ""
echo "按 Ctrl+C 停止所有服务器"

# 清理函数
cleanup() {
    echo ""
    echo "正在停止所有后端服务器..."
    for pid in "${PIDS[@]}"; do
        if kill -0 $pid 2>/dev/null; then
            echo "停止进程 $pid"
            kill $pid
        fi
    done
    echo "所有服务器已停止"
    exit 0
}

# 捕获中断信号
trap cleanup SIGINT SIGTERM

# 等待所有后台进程
wait 