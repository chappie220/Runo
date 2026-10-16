# Runo client

The management client for Runo devices, for Windows, Linux and Android:
it finds devices on the local network, pairs with them, and then shows
their Agent and shells, sessions, status, logs, files and trusted
clients. The network side is the `runo-link` crate (`../link`), the same
code `runo remote` uses, reached through flutter_rust_bridge
(`rust/src/api/client.rs`).

## Building

Needs Flutter 3.47 and a Rust toolchain (plus the Android NDK for
Android, or Visual Studio with C++ for Windows).

```sh
flutter build linux      # build/linux/x64/release/bundle/
flutter build apk        # build/app/outputs/flutter-apk/
flutter build windows    # build/windows/x64/runner/Release/
```

After changing `rust/src/api`, regenerate the bindings:

```sh
cargo install flutter_rust_bridge_codegen --version 2.13.0
flutter_rust_bridge_codegen generate
```

## Notes

- The client's identity key lives in the system's secure store (Android
  Keystore, Windows Credential Manager, the Secret Service on Linux). On
  a Linux desktop without one it falls back to `identity.pem`, readable
  only by the user, in the app's support folder.
- Devices pair only over the local network: the device refuses pairing
  from any other address.
- Android needs the multicast lock the app takes while it looks for
  devices; some Wi-Fi networks still drop mDNS, and pairing by address
  works there.
- On Windows, allow the app through the firewall when asked, or mDNS
  answers do not get in.
