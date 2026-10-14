---
title: Download
description: Download the app for macOS, Windows, or Linux, with install instructions for each.
nav_order: 1
---

{% assign v = site.musify_version %}
{% assign base = "https://github.com/crmne/spotifast/releases/download/v" | append: v %}
{% comment %} Releases after 0.12.0 ship a Windows Installer package (.msi). {% endcomment %}
{% assign version = v | split: "." %}
{% assign major = version[0] | plus: 0 %}
{% assign minor = version[1] | plus: 0 %}
{% assign patch = version[2] | plus: 0 %}
{% assign windows_installer = "-setup.exe" %}
{% if major > 0 or minor > 12 or minor == 12 and patch > 0 %}{% assign windows_installer = ".msi" %}{% endif %}

The current version is **v{{ v }}**. You can use the
[download checksums]({{ base }}/checksums.txt) to check that a file has not
been damaged or changed. Older versions are on the
[releases page](https://github.com/crmne/spotifast/releases).

## macOS

One download for both Apple Silicon and Intel:

- [Download for Mac]({{ base }}/musify-v{{ v }}-macos-universal.dmg)

Open the downloaded file and drag **Musify** to **Applications**.

If you use [Homebrew](https://brew.sh), you can install it with:

```sh
brew install --cask crmne/tap/musify
```

### First open on macOS

Open **Musify** from Applications. If your Mac asks whether you want to
open an app downloaded from the internet, choose **Open**.

Starting with 0.8.0, the Mac download passes Apple's security checks. You do
not need to change security settings or run commands in Terminal to open it.

Quit the old app before opening Musify. Your saved settings and sign-ins
carry over. If an older version reports that its update helper exited before
it was ready, install the DMG above manually once. The helper changes take
effect for updates started from 0.9.1 onward.

## Windows

Download and run the installer. It adds Musify to the Start menu and does
not need an administrator password. Choose the first download for most PCs,
or the ARM version if your PC uses an ARM processor:

- [Windows installer (most PCs)]({{ base }}/musify-v{{ v }}-x86_64-pc-windows-msvc{{ windows_installer }})
- [Windows installer (ARM PCs)]({{ base }}/musify-v{{ v }}-aarch64-pc-windows-msvc{{ windows_installer }})
{% if windows_installer == ".msi" %}
The installer is a Windows Installer package (`.msi`). Musify installed
with the earlier setup program (`-setup.exe`) moves to it through its own
updates, or right away when you run the new installer, and keeps its settings,
its folder and its Start menu entry. Windows then lists one Musify under
installed apps.
{% endif %}
To run Musify without an installer, download a ZIP file, extract it, and
open `musify.exe`.

- [Windows ZIP (most PCs)]({{ base }}/musify-v{{ v }}-x86_64-pc-windows-msvc.zip)
- [Windows ZIP (ARM PCs)]({{ base }}/musify-v{{ v }}-aarch64-pc-windows-msvc.zip)

Either way, SmartScreen may warn about an unknown publisher on first run;
choose **More info**, then **Run anyway**.

Musify needs OpenGL 2.0 or newer, which every current Windows graphics
driver provides. In a virtual machine or remote session that only has the
Microsoft Basic Display Adapter, it cannot open its window and says so in a
message that links to this page; install the machine's graphics driver, or
enable GPU acceleration for the virtual machine.

To choose which app opens Spotify links, use **Settings → Apps → Default apps**
in Windows.

Uninstalling while Musify runs removes nothing until it is closed. The
installer offers to close it for you; closing it that way ends it at once.

## Linux

### Arch Linux

Musify is in the AUR, Arch's community package collection. Installing it
also adds it to your app launcher:

```sh
yay -S musify-bin      # the released build, ready made
yay -S musify          # the release, built from source
yay -S musify-git      # built from the latest commit
```

### Flatpak

Download the [Musify Flatpak]({{ base }}/musify-v{{ v }}-x86_64.flatpak?flatpak-id=io.github.msedge_projects.Musify).
It requires Flatpak and the Freedesktop 25.08 runtime, a set of shared
components used by Flatpak apps:

```sh
flatpak install --user ~/Downloads/musify-v{{ v }}-x86_64.flatpak
flatpak run io.github.msedge_projects.Musify
```

The application ID is **`io.github.msedge_projects.Musify`**.

To update this Flatpak installation, download and install the new release.
Flathub support is planned.

Packages from other stores are maintained by their publishers. Report
problems specific to those packages to their maintainers.

### AppImage

- [AppImage (Intel and AMD, 64-bit)]({{ base }}/musify-{{ v }}-x86_64.AppImage)
- [AppImage (ARM, 64-bit)]({{ base }}/musify-{{ v }}-aarch64.AppImage)

One file, no installation: make it executable and run it.

```sh
chmod +x ~/Downloads/musify-{{ v }}-x86_64.AppImage
~/Downloads/musify-{{ v }}-x86_64.AppImage
```

The AppImage bundles no libraries: like the DEB and RPM, it needs glibc 2.39
or newer and your desktop's own libraries. Running it needs FUSE; without
FUSE, start it with `--appimage-extract-and-run`. It does not update itself:
download the new file when Musify says a release is out.

### Other distributions

- [Linux archive (Intel and AMD, 64-bit)]({{ base }}/musify-v{{ v }}-x86_64-unknown-linux-gnu.tar.gz)
- [Linux archive (ARM, 64-bit)]({{ base }}/musify-v{{ v }}-aarch64-unknown-linux-gnu.tar.gz)

Unpack, put `musify` on your PATH, and copy the desktop entry and icon
from the bundled `packaging/` directory if you want it in your launcher and
handling `spotify:` links.
The binary needs ALSA, PulseAudio or PipeWire, and Wayland or X11.

Or build from source: see the [build instructions](/getting-started/#build-from-source).

## Nix

Add the repository [flake](https://github.com/crmne/spotifast) to your
inputs:

```nix
inputs.musify.url = "github:crmne/spotifast";
```

On NixOS, install the default package:

```nix
environment.systemPackages = [
  inputs.musify.packages."${pkgs.stdenv.hostPlatform.system}".default
];
```

### nix-darwin

On macOS, use the same `musify` package to install Musify as a Mac app:

```nix
environment.systemPackages = [
  inputs.musify.packages."${pkgs.stdenv.hostPlatform.system}".musify
];
environment.pathsToLink = [ "/Applications" ];
```

The bundle appears in `/Applications/Nix Apps`. With Home Manager,
`home.packages` is enough; its darwin support links app bundles into
`~/Applications`:

```nix
home.packages = [
  inputs.musify.packages."${pkgs.stdenv.hostPlatform.system}".musify
];
```
