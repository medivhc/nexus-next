# verify-nexus feature map

Index of user-facing features covered by this verification skill. Each feature file documents the user journey, how to drive it with the skill's helpers, and gotchas.

| Feature | User journey | Primary helper | Status |
|---------|--------------|----------------|--------|
| [network-init-join](./network-init-join.md) | `nexus init` → `nexus join` → `nexus device approve` → both online | `cli-drive.sh` | ✅ seeded |
| [persistent-terminal](./persistent-terminal.md) | `nexus ssh home` / `nexus exec home -- ...` → session survives disconnect | `cli-drive.sh` | ✅ seeded |
| [web-enroll-login](./web-enroll-login.md) | `nexus web enroll` → browser passkey enroll → login → terminal in browser | `web-drive.mjs` | ✅ seeded |
| [drive-sync](./drive-sync.md) | `nexus drive push/pull` files between devices | `cli-drive.sh` | ✅ seeded |
| [anchor-relay](./anchor-relay.md) | laptop `--anchor-dev` serves page + relay; home `--relay` connects through it | `launch-daemons.sh` + `web-drive.mjs` | ✅ seeded |

**Adding a feature:** Create `<feature-name>.md` with the four H2s: `Sub-features`, `How to get to it (user POV)`, `Driving it with verify-nexus`, `Gotchas`. Update this README.

**Maintenance:** Run `/maintain-verification-skill verify-nexus` when flows change.