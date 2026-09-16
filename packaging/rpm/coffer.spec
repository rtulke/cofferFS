# Source RPM spec for COPR (Fedora, EL 9/10) and other rpmbuild-based
# services. The release workflow's binary RPMs come from cargo-generate-rpm
# instead (see Cargo.toml); this spec exists so a repository like COPR can
# build coffer from source on its own infrastructure and offer it via
# `dnf install coffer`.
#
# Builds with the distro's rust/cargo and needs network access during
# %%build for `cargo build` to fetch crates (COPR: enable "Internet access
# during build" for the project). No libfuse development package: fuser's
# pure-Rust mount links none. SQLCipher and OpenSSL are compiled from
# source and statically linked, hence perl and make.
#
# Bump Version, %%commit and %%changelog together on every release; the
# "Source packages" workflow builds and smoke-tests this file on Fedora and
# EL whenever it changes and on every release tag.

%global commit d42a99f74636a34e420ccaae14f620cc8c27c7cb
%global shortcommit %(c=%{commit}; echo ${c:0:7})
# The release profile strips the binary (Cargo.toml), so find-debuginfo has
# nothing to extract and EL's rpmbuild aborts on the empty debug source
# list. No debuginfo subpackage, then.
%global debug_package %{nil}

Name:           coffer
Version:        0.1.2
Release:        1%{?dist}
Summary:        Growable, user-mountable encrypted single-file containers

License:        MIT
URL:            https://github.com/rtulke/cofferFS
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz#/cofferFS-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  perl
# The fusermount3 helper that mount/umount shell out to at runtime.
Requires:       fuse3

%description
coffer creates a single-file encrypted container that any user can create
and mount without root or sudo. It is backed by SQLCipher (bundled,
statically linked) and mounted via FUSE. Containers grow automatically as
data is written (no fixed size, no resize step) and are crash-safe via
SQLite's WAL journal.

%prep
%autosetup -n cofferFS-%{version}

%build
export COFFER_GIT_HASH=%{shortcommit}
export GITHUB_SHA=%{commit}
cargo build --release --locked
target/release/coffer completions bash > coffer.bash
target/release/coffer completions zsh  > _coffer
target/release/coffer completions fish > coffer.fish

%install
install -Dm755 target/release/coffer %{buildroot}%{_bindir}/coffer
# rpmbuild compresses man pages itself (brp-compress), so install it plain.
install -Dm644 packaging/coffer.1 %{buildroot}%{_mandir}/man1/coffer.1
install -Dm644 coffer.bash %{buildroot}%{_datadir}/bash-completion/completions/coffer
install -Dm644 _coffer     %{buildroot}%{_datadir}/zsh/site-functions/_coffer
install -Dm644 coffer.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/coffer.fish

%check
cargo test --release --locked

%files
%license LICENSE
%doc README.md REFERENCE.md
%{_bindir}/coffer
%{_mandir}/man1/coffer.1*
%{_datadir}/bash-completion/completions/coffer
%{_datadir}/zsh/site-functions/_coffer
%{_datadir}/fish/vendor_completions.d/coffer.fish

%changelog
* Wed Sep 17 2026 Robert Tulke <rt@debian.sh> - 0.1.2-1
- Read-only mounts, mount logs, --password-command, hardened process,
  VACUUM copy on disk, signed release checksums

* Tue Sep 16 2026 Robert Tulke <rt@debian.sh> - 0.1.1-1
- Initial package
