# Security Policy

## Supported versions

Broom ships as rolling releases from `main` (tagged with the push time in UTC, e.g. `2026-09-19T17-05-25Z`). Security fixes go into the **latest
release** only; please update before reporting, and check whether the issue still happens there.

| Version | Supported |
|---|---|
| latest release | ✅ |
| anything older | ❌ — update to the latest release |

## Reporting a vulnerability

**Please do not open a public issue, discussion or pull request for a security problem.**

Report it privately through GitHub: repository → **Security** → **Report a vulnerability**
([private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)).
Include:

- the affected version and component (web/API, iSCSI daemon, DHCP/TFTP, Windows stage, prep / client scripts);
- how to reproduce it, and what an attacker gains (from where: the boot LAN, the internet, a client machine);
- a proof of concept if you have one — redact real keys, passwords and addresses.

What to expect:

- an acknowledgement within **7 days**;
- an assessment and, for a confirmed issue, a fix plan within **30 days** — sooner for anything remotely exploitable;
- a GitHub security advisory published together with the fixed release, crediting you unless you prefer otherwise.

Please give us reasonable time to release a fix before disclosing details publicly.

## Scope and known limits

In scope: anything that lets someone **bypass the admin login**, run code on the server, read or change data they
shouldn't (license keys, the guest password, other machines' sessions), break the reset of a client between
sessions, or take the server down from a client machine or the LAN with little effort.

Some properties come with diskless boot on a shared LAN and are documented, not bugs (README → **Security**):

- the iSCSI portal, TFTP and the goldens over HTTP are readable by any host on the boot LAN — that is how clients
  boot; the boot LAN must be segmented from guest Wi-Fi and unknown devices;
- license keys are handed out once over plain HTTP, to a registered machine identified by its fixed IP + ARP;
- HTTPS covers the web admin only. What the PCs fetch stays plain HTTP on the boot LAN: boot files, scripts, and a
  machine's Remote agent key (once, on a first boot — an SSD machine's first start after it was registered or its key
  re-issued, a diskless machine's each time it builds its base). Someone listening on that LAN at that moment can read
  the key; commands to the agent are signed with it. No HSTS header is sent (browsers ignore it for a certificate they
  do not trust, and the admin can be put back on plain HTTP);
- games disk writes are accepted from the update machine's IP only (iSCSI carries no other proof of identity);
- a diskless machine (no SSD) is its fixed IP + the MAC the server sees for it (ARP), nothing more: its own disk — the
  golden, its base, its session — its Remote agent key and its first-boot "setup done" are served to whatever answers
  at that address. Diskless rooms are taken to be a controlled network (the server, the PCs, a managed switch). A
  device that takes a PC's IP and MAC while it is off can read that PC's base and, after a new golden version, do
  its first boot in its place; nothing of the golden or of other machines is touched, every refused attempt is in
  the log and at the top of the admin pages, and **Set up again** (Diskless page) rebuilds that machine's base.
  Port isolation or ARP inspection on the boot LAN's switch closes it;
- the ports a boot needs are open to every host on the boot LAN before any check, so what one host can make the
  server hold there is bounded, not prevented: a request that needs no sign-in is at most 256 KB and 60 s; one
  address gets at most 256 web connections (4096 in all) and 64 iSCSI connections; iSCSI login text and ping data
  are capped. A host that fills its share is logged; many addresses together can still keep others out for as long
  as they hold on — the boot LAN's switch (port security, one address per port) is what closes that;
- the guest account is a local administrator on Windows clients (games need it): a determined guest can interfere
  with the machine they sit at until its next reset.

Reports that show one of these is weaker than documented (e.g. reachable from outside the boot LAN, or affecting
other machines) are in scope.
