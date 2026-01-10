#!/bin/bash
set -e

echo "🔧 Setting up NVIDIA Container Toolkit..."

# 1. Add NVIDIA package repository
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg \
  && curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list | \
    sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
    sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list

# 2. Update and Install
sudo apt-get update
sudo apt-get install -y nvidia-container-toolkit

# 3. Configure Docker
echo "⚙️ Configuring Docker Runtime..."
sudo nvidia-ctk runtime configure --runtime=docker

# 4. Restart Docker
echo "🔄 Restarting Docker Daemon..."
sudo systemctl restart docker

echo "✅ Done! Testing GPU access in Docker..."
docker run --rm --runtime=nvidia --gpus all ubuntu nvidia-smi
