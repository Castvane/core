# Restreamer UI browser regression

The harness builds the five unchanged [compatibility pins](BOOTSTRAP.md#pinned-compatibility-references) and drives their production bundles in Chromium against a disposable Castvane node. The browser runs on loopback. Captured diagnostics contain API methods, paths and statuses. They omit query strings, submitted values, tokens and response bodies.

Install Node.js 24, Corepack, Git and Chromium. Legacy fixtures use Yarn Classic, and the owner fixture uses the pnpm version in its package manifest. Build in a temporary location with room for about 2 GiB of dependencies.

```sh
cargo build --locked --release
bash scripts/build-ui-fixture.sh v1.14.0 /tmp/castvane-ui-v1.14.0
node scripts/test-ui-fixture.mjs target/release/restreamer-core /tmp/castvane-ui-v1.14.0
```

Use `v1.0.0`, `v1.8.0`, `v1.14.0`, `upstream` or `owner` as the pin. The harness tries a bad password first, signs in with test credentials and reloads the page to check refresh. A successful discovery and login run can still report `versionGate: true`. Confirm the dashboard result separately.

| Pin | Build | Browser result |
| --- | --- | --- |
| Upstream v1.0.0 | Failed on Linux | The source imports `./logos/red5.svg` from `Red5.js`. The actual directory is `Logos` with an uppercase `L`. |
| Upstream v1.8.0 | Passed | Discovery, rejected bad credentials, login and refresh passed. The Core version gate appeared before configuration loaded. |
| Upstream v1.14.0 | Passed | Same result as v1.8.0. |
| Upstream HEAD `7164b38` | Passed | Same result as v1.8.0. |
| Owner main `69feb33` | Passed | Discovery and form rendered. The unchanged form never sent `POST /api/login` during the credential attempt. The pinned password component supplies a controlled empty default, and the form does not supply a value. |

The upstream pins that signed in also requested the optional `restreamer-ui` metadata key. A fresh instance has no saved value and returned 404. The same instance still refreshed the session successfully.

Full dashboard, process control, media playback, settings and administration workflows remain gated. Manual acceptance and the remaining v1 tags are open. Castvane continues to report its application version.
