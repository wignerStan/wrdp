# Third-party software

WRDP depends on software with its own copyright and license terms.

## IronRDP

WRDP uses [IronRDP](https://github.com/Devolutions/IronRDP) for RDP protocol state machines, capability negotiation, protocol data units and dynamic channels.

The IronRDP dependency is pinned to revision `b67d44f1312ecf144d3511fc8a54f3b3ff2d4847` from [`wignerStan/IronRDP`](https://github.com/wignerStan/IronRDP), based on upstream revision `e4143f26306a6639a889d80c1c484ca69c365a8f`. The fork adds ordered RDPSND teardown before server-initiated disconnects and submits AVC444 V1 main and auxiliary views in one frame through the borrowed encoder API. IronRDP is licensed under Apache-2.0.

## Managed compositor

The managed compositor source is derived from [labwc 0.8.3](https://github.com/labwc/labwc/releases/tag/0.8.3) and is licensed under GPL-2.0-only. Its complete source, license, original file notices and upstream comparison record are under `vendor/wrdp-compositor/`. The two application icons retain Johan Malm's CC BY-SA 4.0 notice; see `vendor/wrdp-compositor/ASSET_LICENSES.md`. The Platinum theme is WRDP artwork recreated from emulator screenshots.

Packaging builds the compositor as a separate executable at `/usr/lib/wrdp/wrdp-compositor`; WRDP starts it as a managed per-user process. It is not linked into the MIT-licensed Rust binaries.

## OpenH264

Software H.264 support uses the Rust `openh264` crate in dynamic-loading mode. Shipped builds load a separately installed OpenH264 library; they do not compile OpenH264 from source.

Cisco publishes binary OpenH264 releases under its binary license and covers MPEG LA royalties for binaries downloaded from Cisco. Distributors and operators are responsible for installing a suitable library under terms that apply to their package or deployment.

The `h264-source` Cargo feature is for development only and must not be enabled in release packaging.

## Other Rust dependencies

Rust dependency licenses are recorded in Cargo package metadata and the lockfile. Release CI generates a machine-readable dependency and license report from the locked graph. Packages with missing, unknown or disallowed licenses block a release.
