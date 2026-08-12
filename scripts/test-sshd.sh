#!/bin/bash
# ==================== kun 测试 sshd 管理脚本 ====================
# 用法：
#   scripts/test-sshd.sh start   启动测试 sshd（127.0.0.1:2222，公钥认证 + sftp）
#   scripts/test-sshd.sh stop    停止测试 sshd
#   scripts/test-sshd.sh status  查看状态
#
# 说明：用于集成测试（crates/kun-core/tests/），不修改系统配置，
# 全部文件在 /tmp/kun-test-sshd/ 下，用 ~/.ssh/id_ed25519 公钥认证。

set -e

SSHD_CONFIG=/tmp/kun-test-sshd/sshd_config
SSHD_PID=/tmp/kun-test-sshd/sshd.pid
SSHD_LOG=/tmp/kun-test-sshd/sshd.log
PORT=${KUN_TEST_PORT:-2222}

start() {
    if [ -f "$SSHD_PID" ] && kill -0 "$(cat "$SSHD_PID")" 2>/dev/null; then
        echo "测试 sshd 已在运行（PID $(cat "$SSHD_PID")）"
        return 0
    fi

    mkdir -p /tmp/kun-test-sshd

    # 1. 生成测试主机密钥（已存在则跳过）
    if [ ! -f /tmp/kun-test-sshd/hostkey ]; then
        ssh-keygen -t ed25519 -f /tmp/kun-test-sshd/hostkey -N "" -q
        echo "已生成测试主机密钥"
    fi

    # 2. 授权当前用户公钥（优先 id_ed25519，其次 id_rsa）
    PUB_KEY=""
    for k in ~/.ssh/id_ed25519.pub ~/.ssh/id_rsa.pub; do
        if [ -f "$k" ]; then PUB_KEY="$k"; break; fi
    done
    if [ -z "$PUB_KEY" ]; then
        echo "错误：未找到 ~/.ssh/id_ed25519.pub 或 ~/.ssh/id_rsa.pub，请先 ssh-keygen 生成密钥" >&2
        exit 1
    fi
    cp "$PUB_KEY" /tmp/kun-test-sshd/authorized_keys
    echo "已授权公钥：$PUB_KEY"

    # 3. 写 sshd 配置（公钥认证 + sftp subsystem + 非特权端口）
    cat > "$SSHD_CONFIG" << EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey /tmp/kun-test-sshd/hostkey
AuthorizedKeysFile /tmp/kun-test-sshd/authorized_keys
PasswordAuthentication no
PubkeyAuthentication yes
PermitRootLogin no
UsePAM no
StrictModes no
PidFile $SSHD_PID
LogLevel VERBOSE
Subsystem sftp /usr/libexec/sftp-server
EOF

    # 4. 启动
    /usr/sbin/sshd -f "$SSHD_CONFIG" -E "$SSHD_LOG"
    sleep 1

    # 5. 验证连通性
    USER=$(whoami)
    if ssh -p "$PORT" -o ConnectTimeout=3 -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes localhost "echo ok" > /dev/null 2>&1; then
        printf "测试 sshd 已启动：127.0.0.1:%s（用户 %s，公钥认证）\n" "$PORT" "$USER"
    else
        echo "警告：sshd 已启动但 SSH 验证失败，查看日志：$SSHD_LOG" >&2
    fi
}

stop() {
    if [ -f "$SSHD_PID" ]; then
        kill "$(cat "$SSHD_PID")" 2>/dev/null || true
        rm -f "$SSHD_PID"
        echo "测试 sshd 已停止"
    else
        echo "测试 sshd 未在运行"
    fi
    # 清理残留的测试会话
    pkill -f "sshd-session:.*@ttys" 2>/dev/null || true
}

status() {
    if [ -f "$SSHD_PID" ] && kill -0 "$(cat "$SSHD_PID")" 2>/dev/null; then
        printf "运行中（PID %s，端口 %s）\n" "$(cat "$SSHD_PID")" "$PORT"
    else
        echo "未运行"
    fi
}

case "${1:-}" in
    start) start ;;
    stop) stop ;;
    status) status ;;
    *)
        echo "用法：$0 {start|stop|status}"
        exit 1
        ;;
esac
