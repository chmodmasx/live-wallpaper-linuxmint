# Dockerfile for building the .deb package

FROM ubuntu:24.04

WORKDIR /app

# Prevent interactive prompts
ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    curl \
    libgtk-4-dev \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgdk-pixbuf-2.0-dev \
    libx11-dev \
    libxrandr-dev \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install Rust using rustup
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install cargo-deb
RUN cargo install cargo-deb

# Set the default command to build the deb
CMD ["cargo", "deb"]
