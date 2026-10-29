FROM base
ARG AGENT_USER=agent

RUN rustup default stable
RUN rustup component add rust-src rust-analyzer clippy rustfmt

RUN claude --version \
    && claude --version \
    && rustc --version \
    && cargo --version \
    && rustfmt --version \
    && cargo clippy --version
