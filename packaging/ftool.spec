Name:           ftool
Version:        0.1.1
Release:        1%{?dist}
Summary:        Fedora system utility for NVIDIA Optimus GPU switching

License:        GPL-3.0-or-later
URL:            https://github.com/red-blakTree/ftool
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc

%description
ftool is a Fedora-oriented command line utility for managing NVIDIA Optimus
dual-GPU laptops. It provides GPU mode switching, runtime power control,
kernel Secure Boot signing, Fedora release upgrades, and file hashing.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked

%install
install -Dpm0755 target/release/ftool %{buildroot}%{_bindir}/ftool
install -Dpm0644 completions/ftool.bash %{buildroot}%{_datadir}/bash-completion/completions/ftool
install -Dpm0644 completions/ftool.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/ftool.fish
install -Dpm0644 LICENSE %{buildroot}%{_licensedir}/ftool/LICENSE

%files
%license LICENSE
%{_bindir}/ftool
%{_datadir}/bash-completion/completions/ftool
%{_datadir}/fish/vendor_completions.d/ftool.fish

%changelog
* Fri Jan 01 2026 ftool maintainers <binarytreerust@outlook.com> - 0.1.1-1
- Initial RPM packaging
