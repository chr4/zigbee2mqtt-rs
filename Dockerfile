FROM rust

# Install npm, sudo
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        sudo \
        curl git ripgrep jq # tools for better token usage \
    && rm -rf /var/lib/apt/lists/*


# Create a non-root user and grant passwordless sudo (so Claude can install things)
RUN useradd --create-home --shell /bin/bash agent \
    && echo "agent ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/agent \
    && chmod 0440 /etc/sudoers.d/agent

RUN rustup default stable
RUN rustup component add rust-src rust-analyzer clippy rustfmt

# Switch to the non-root user
USER agent
WORKDIR /src

# Claude is installed in the user's home directory by default
RUN mkdir -p ~/.local/bin
ENV PATH="$PATH:/home/agent/.local/bin"

ENV CLAUDE_CONFIG_DIR=/home/agent/.claude-state
RUN curl -fsSL https://claude.ai/install.sh | bash

# Install external tools for better token usage
RUN curl -fsSL https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh | bash
RUN curl -sSL https://mqlang.org/install.sh | bash
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# Verify all tools are available.
RUN claude --version \
    && claude --version \
    && rustc --version \
    && cargo --version \
    && rustfmt --version \
    && cargo clippy --version
