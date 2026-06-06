FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    openssh-server \
    systemd \
    systemd-sysv \
    cron \
    e2fsprogs \
    xfsprogs \
    util-linux \
    sudo \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Configure SSH
RUN mkdir -p /run/sshd /root/.ssh && \
    chmod 700 /root/.ssh && \
    sed -i 's/#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config

# Inject public key via build arg
ARG PUBKEY
RUN echo "${PUBKEY}" > /root/.ssh/authorized_keys && \
    chmod 600 /root/.ssh/authorized_keys

# Non-root user for run-as (privilege escalation) tests: passwordless sudo and the
# same authorized key, so tests can connect as `deploy` and escalate to root.
RUN useradd -m -s /bin/bash deploy && \
    mkdir -p /home/deploy/.ssh && \
    echo "${PUBKEY}" > /home/deploy/.ssh/authorized_keys && \
    chmod 700 /home/deploy/.ssh && \
    chmod 600 /home/deploy/.ssh/authorized_keys && \
    chown -R deploy:deploy /home/deploy/.ssh && \
    echo "deploy ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/deploy && \
    chmod 440 /etc/sudoers.d/deploy

# Create a 64MB loopback image for disk module tests
RUN dd if=/dev/zero of=/opt/fake.img bs=1M count=64

# Expose SSH port
EXPOSE 22

# Use systemd as init (requires --privileged)
CMD ["/sbin/init"]
