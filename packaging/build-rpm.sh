#!/usr/bin/env bash
set -euo pipefail

# 在 Fedora 容器内执行：安装 RPM 构建依赖并生成 RPM
dnf -y install \
    git \
    gcc \
    make \
    cargo \
    rust \
    rpm-build

VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)"
if [[ -z "$VERSION" ]]; then
    echo "无法从 Cargo.toml 解析版本号" >&2
    exit 1
fi

TOP_DIR="$PWD/dist/rpmbuild"
rm -rf "$TOP_DIR"
mkdir -p "$TOP_DIR"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

# 用 git archive 生成规范的源码 tar 包，避免把 target/ 等本地产物带入 RPM
git archive \
    --format=tar \
    --prefix="ftool-$VERSION/" \
    HEAD |
    gzip >"$TOP_DIR/SOURCES/ftool-$VERSION.tar.gz"

cp packaging/ftool.spec "$TOP_DIR/SPECS/ftool.spec"

rpmbuild \
    --define "_topdir $TOP_DIR" \
    -bb \
    "$TOP_DIR/SPECS/ftool.spec"

find "$TOP_DIR/RPMS" -name '*.rpm' -print
