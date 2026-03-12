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

# Create a 64MB loopback image for disk module tests
RUN dd if=/dev/zero of=/opt/fake.img bs=1M count=64

# Expose SSH port
EXPOSE 22

# Use systemd as init (requires --privileged)
CMD ["/sbin/init"]
