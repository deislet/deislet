#!/bin/bash
# Deflux 平滑重启集成测试脚本

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# 测试配置
TEST_PORT=18080
TEST_CONFIG="/tmp/deflux-test-config.toml"
TEST_DB="/tmp/deflux-test.db"
DEFLUX_BIN="./target/debug/deflux"
LOG_FILE="/tmp/deflux-test.log"

# 清理函数
cleanup() {
    echo -e "${YELLOW}清理测试环境...${NC}"
    pkill -f "deflux.*${TEST_CONFIG}" || true
    rm -f "$TEST_CONFIG" "$TEST_DB" "$LOG_FILE"
    rm -f /tmp/deflux-restart.sock
}

# 设置清理 trap
trap cleanup EXIT

# 检查二进制文件
check_binary() {
    if [ ! -f "$DEFLUX_BIN" ]; then
        echo -e "${RED}错误: Deflux 二进制文件不存在${NC}"
        echo "请先编译: cargo build -p deflux"
        exit 1
    fi
}

# 创建测试配置
create_test_config() {
    echo -e "${GREEN}创建测试配置...${NC}"
    cat > "$TEST_CONFIG" <<EOF
[system]
listen_addr = ["127.0.0.1"]
listen_ports = [$TEST_PORT]

[system.log]
[system.log.error_log]
level = "Debug"

[system.tokio_runtime]
worker_threads = 2
EOF
}

# 启动 Deflux
start_deflux() {
    echo -e "${GREEN}启动 Deflux (端口: $TEST_PORT)...${NC}"
    RUST_LOG=deflux=debug "$DEFLUX_BIN" -c "$TEST_CONFIG" > "$LOG_FILE" 2>&1 &
    DEFLUX_PID=$!

    # 等待启动
    sleep 2

    if ! kill -0 $DEFLUX_PID 2>/dev/null; then
        echo -e "${RED}错误: Deflux 启动失败${NC}"
        cat "$LOG_FILE"
        exit 1
    fi

    echo -e "${GREEN}Deflux 已启动 (PID: $DEFLUX_PID)${NC}"
}

# 测试连接
test_connection() {
    local expected_pid=$1
    echo -e "${YELLOW}测试连接到 127.0.0.1:$TEST_PORT...${NC}"

    # 使用 netcat 或 curl 测试
    if command -v nc &> /dev/null; then
        timeout 2 bash -c "echo 'GET / HTTP/1.0\r\n\r\n' | nc 127.0.0.1 $TEST_PORT" > /dev/null 2>&1 || true
    elif command -v curl &> /dev/null; then
        curl -s --max-time 2 "http://127.0.0.1:$TEST_PORT/" > /dev/null 2>&1 || true
    else
        echo -e "${YELLOW}警告: nc 和 curl 都不可用，跳过连接测试${NC}"
        return 0
    fi

    echo -e "${GREEN}连接测试通过${NC}"
}

# 触发平滑重启
trigger_graceful_restart() {
    echo -e "${GREEN}触发平滑重启 (发送 SIGUSR1)...${NC}"
    kill -USR1 $DEFLUX_PID

    # 等待一小段时间让新进程启动
    sleep 3

    # 查找新进程
    NEW_PID=$(pgrep -f "deflux.*${TEST_CONFIG}" | grep -v "^${DEFLUX_PID}$" | head -1 || true)

    if [ -z "$NEW_PID" ]; then
        echo -e "${RED}错误: 未找到新进程${NC}"
        echo -e "${YELLOW}旧进程 PID: $DEFLUX_PID${NC}"
        echo -e "${YELLOW}日志内容:${NC}"
        tail -50 "$LOG_FILE"
        exit 1
    fi

    echo -e "${GREEN}新进程已启动 (PID: $NEW_PID)${NC}"
    echo -e "${YELLOW}旧进程 PID: $DEFLUX_PID, 新进程 PID: $NEW_PID${NC}"

    # 等待旧进程退出
    local wait_count=0
    while kill -0 $DEFLUX_PID 2>/dev/null; do
        sleep 1
        wait_count=$((wait_count + 1))
        if [ $wait_count -gt 10 ]; then
            echo -e "${YELLOW}警告: 旧进程 10 秒后仍未退出${NC}"
            break
        fi
    done

    if kill -0 $DEFLUX_PID 2>/dev/null; then
        echo -e "${YELLOW}旧进程仍在运行，可能在等待连接关闭${NC}"
    else
        echo -e "${GREEN}旧进程已退出${NC}"
    fi

    # 更新 PID 为新进程
    DEFLUX_PID=$NEW_PID
}

# 验证日志
verify_logs() {
    echo -e "${YELLOW}验证日志...${NC}"

    local expected_logs=(
        "Received SIGUSR1, initiating graceful restart"
        "Extracted.*FDs for transfer"
        "Spawned new process with PID"
        "Detected graceful restart, receiving listeners from parent"
        "Child process: received.*FDs"
        "successfully rebuilt.*listeners"
    )

    local missing_logs=0
    for log_pattern in "${expected_logs[@]}"; do
        if ! grep -q "$log_pattern" "$LOG_FILE"; then
            echo -e "${RED}  ✗ 缺少日志: $log_pattern${NC}"
            missing_logs=$((missing_logs + 1))
        else
            echo -e "${GREEN}  ✓ 找到日志: $log_pattern${NC}"
        fi
    done

    if [ $missing_logs -gt 0 ]; then
        echo -e "${RED}警告: 有 $missing_logs 条预期日志未找到${NC}"
        echo -e "${YELLOW}完整日志:${NC}"
        cat "$LOG_FILE"
        return 1
    fi

    echo -e "${GREEN}日志验证通过${NC}"
}

# 主测试流程
main() {
    echo -e "${GREEN}========================================${NC}"
    echo -e "${GREEN}Deflux 平滑重启集成测试${NC}"
    echo -e "${GREEN}========================================${NC}"

    # 步骤 1: 检查和准备
    check_binary
    cleanup
    create_test_config

    # 步骤 2: 启动初始进程
    start_deflux
    OLD_PID=$DEFLUX_PID

    # 步骤 3: 测试初始连接
    test_connection $OLD_PID

    # 步骤 4: 触发平滑重启
    trigger_graceful_restart

    # 步骤 5: 测试重启后的连接
    sleep 2
    test_connection $DEFLUX_PID

    # 步骤 6: 验证日志
    if verify_logs; then
        echo -e "${GREEN}========================================${NC}"
        echo -e "${GREEN}✓ 所有测试通过！${NC}"
        echo -e "${GREEN}========================================${NC}"
    else
        echo -e "${YELLOW}========================================${NC}"
        echo -e "${YELLOW}⚠ 测试完成，但有警告${NC}"
        echo -e "${YELLOW}========================================${NC}"
    fi

    # 步骤 7: 清理
    echo -e "${YELLOW}停止 Deflux...${NC}"
    kill -TERM $DEFLUX_PID 2>/dev/null || true
    sleep 2

    echo -e "${GREEN}测试完成${NC}"
}

# 运行主测试
main
