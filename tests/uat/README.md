# Acceptance checks

`bash scripts/test-image.sh IMAGE` runs packaged Pi protocol, worker isolation,
verification, protected-file, repair, cancellation, restart, deadline, networking
and wake-relay fixtures. Use a disposable Docker Engine 28+ host. The network
permission fixture downloads `is-number@7.0.0`; model responses are local protocol
fixtures. These checks do not claim physical GPU performance or Wake-on-LAN.

`python3 scripts/test-install.py IMAGE` checks fresh Compose installation, saved
configuration, restart, stopped backup and restoration into new volumes. An
optional `--previous-image IMAGE` also checks upgrade and rollback. It removes
only the fresh test projects it created.

`python3 scripts/test-browser.py` requires Playwright 1.62.1 and Chromium.
Set `POUNDLOCK_TEST_CONTROLLER` to a native binary, or build `target/debug/poundlock`.
The suite starts disposable loopback controllers and tests onboarding downloads,
hardware guidance, selection preservation, Connections/Models editing/removal,
jobs-only permissions, source review, retry after lost responses, patch
downloads, safe result rendering, the Catppuccin theme menu and mobile layout.
Mocked model and runner results are labeled as fixtures. See the release
workflow for dependency installation.

`tests/uat/linux-node.py` is a separate privileged fixture for a disposable
systemd Linux VM. It refuses an existing installation. Run its initial pass,
reboot the VM, then use `--reconnect` to verify startup and removal; see its usage
header. Never run it on an enrolled production inference machine.

Physical acceptance should additionally exercise a sleeping node: confirm it is
offline, request inference, require an actual completed response, then confirm
lease release and expected model unloading. Avoid SSH probes while testing wake.
Use existing models to validate workflows; model qualification is a separate task.
