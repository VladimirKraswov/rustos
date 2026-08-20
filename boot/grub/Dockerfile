# GRUB host tools do not have a native macOS build. This small, cached image
# makes the exact same x86_64 EFI standalone image on Apple Silicon and Linux.
FROM debian:13.1-slim@sha256:a347fd7510ee31a84387619a492ad6c8eb0af2f2682b916ff3e643eb076f925a

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       grub-common=2.12-9+deb13u2 grub-efi-amd64-bin=2.12-9+deb13u2 \
    && rm -rf /var/lib/apt/lists/*
