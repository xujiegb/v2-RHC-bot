Name: rhc-bot
Version: %{version}
Release: 1%{?dist}
Summary: Telegram RHC activation-key verification bot
License: MIT
Requires: podman
%description
A lightweight Rust Telegram gatekeeper which validates RHC activation keys in disposable UBI containers.
%install
install -Dpm0755 %{binary} %{buildroot}%{_bindir}/rhc-bot
install -Dpm0644 %{service} %{buildroot}%{_unitdir}/rhc-bot.service
%post
%systemd_post rhc-bot.service
%preun
%systemd_preun rhc-bot.service
%postun
%systemd_postun_with_restart rhc-bot.service
%files
%license LICENSE
%{_bindir}/rhc-bot
%{_unitdir}/rhc-bot.service
