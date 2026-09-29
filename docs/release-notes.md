Terminal Agent with encrypted device collaboration. This is an early prototype release.

## Downloads

| Platform | Archive |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin.tar.gz` |
| macOS Intel | `x86_64-apple-darwin.tar.gz` |
| Linux x64, glibc 2.35+ | `x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64, glibc 2.35+ | `aarch64-unknown-linux-gnu.tar.gz` |
| Windows x64 | `x86_64-pc-windows-msvc.zip` |

Each archive contains the standalone executable, documentation, dependency lockfile and license notices. No Rust or Node.js is needed to run it.

Download `install.sh` (macOS/Linux) or `install.ps1` (Windows) from this release, then run it. The installer verifies the archive against `SHA256SUMS` before replacing an existing installation. Private repositories require GitHub access, `gh auth login`, and `MUTLINK_USE_GH=1`.

`mutlink.rb` is the Homebrew formula generated from these exact archives. Its download URLs require publicly accessible release assets for ordinary Homebrew installation.

Stop the existing daemon with `mutlink stop` before upgrading. Installation does not modify agent data or model configuration. Binaries are not code-signed or notarized. See the README for setup, access controls and current limitations.
