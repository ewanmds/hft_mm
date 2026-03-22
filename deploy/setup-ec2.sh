#!/bin/bash
# Run this script on your EC2 instance to set up the HFT bot
set -e

echo "=== Installing Rust ==="
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

echo "=== Building the bot ==="
cd ~/hft_mm
cargo build --release

echo "=== Setting up .env ==="
if [ ! -f .env ]; then
  cat > .env << 'EOF'
HL_AGENT_KEY=0x_YOUR_AGENT_PRIVATE_KEY
HL_ACCOUNT=0x_YOUR_ACCOUNT_ADDRESS
API_KEY=change_this_to_a_strong_secret
HEADLESS=1
RUST_LOG=info
EOF
  echo "Created .env — EDIT IT with your credentials before starting!"
fi

echo "=== Installing systemd service ==="
sudo cp deploy/hft-mm.service /etc/systemd/system/hft-mm.service
sudo sed -i "s|/home/ubuntu|$HOME|g" /etc/systemd/system/hft-mm.service
sudo systemctl daemon-reload
sudo systemctl enable hft-mm

echo ""
echo "=== Setup complete ==="
echo "1. Edit .env with your HL_AGENT_KEY, HL_ACCOUNT, and API_KEY"
echo "2. Start the bot:  sudo systemctl start hft-mm"
echo "3. Check logs:     sudo journalctl -u hft-mm -f"
echo "4. API running at: http://$(curl -s ifconfig.me):3001"
