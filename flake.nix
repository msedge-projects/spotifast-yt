{
  description = "Spotify, native and fast";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # rust-toolchain.toml pins the compiler so local builds and CI agree.
    # This reads that file rather than restating the version here.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            }
          )
        );
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              rust-analyzer
              pkg-config
              # libprojectM (MilkDrop) is built from source by CMake, and its
              # bindings by bindgen, which needs libclang.
              cmake
              rustPlatform.bindgenHook
            ]
            ++ lib.optionals stdenv.hostPlatform.isDarwin [
              apple-sdk
            ]
            ++ lib.optionals stdenv.hostPlatform.isLinux [
              dbus
              alsa-lib
              libpulseaudio
              libxkbcommon
              wayland
              libGL
              libx11
              libxcursor
              libxi
              libxrandr
            ];
          # The GUI dlopens its Wayland, X11 and GL libraries at run time.
          LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux (
            pkgs.lib.makeLibraryPath (
              with pkgs;
              [
                dbus
                libxkbcommon
                wayland
                libGL
                libx11
                libxcursor
                libxi
                libxrandr
              ]
            )
          );
        };
      });

      packages = forAllSystems (
        pkgs:
        let
          musify =
            let
              toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
              rustPlatform = pkgs.makeRustPlatform {
                cargo = toolchain;
                rustc = toolchain;
              };
              runtimeLibs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
                with pkgs;
                [
                  dbus
                  libxkbcommon
                  wayland
                  libGL
                  libx11
                  libxcursor
                  libxi
                  libxrandr
                ]
              );
            in
            rustPlatform.buildRustPackage rec {
              pname = "musify";
              version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
              src = self;

              # The lock file contains git dependencies. fetchCargoVendor includes
              # them in the fixed-output dependency tree, unlike cargoLock alone.
              cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
                pname = "musify";
                version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
                src = self;
                # The vendor tree changed with the decode dependencies; this
                # placeholder makes the next build report the value.
                hash = "";
              };
              # projectm-sys only searches lib, while CMake may otherwise install to lib64.
              postPatch = ''
                substituteInPlace "$cargoDepsCopy"/source-git-*/projectm-sys-*/build.rs \
                  --replace-fail \
                  '.define("BUILD_SHARED_LIBS", build_shared_libs)' \
                  '.define("CMAKE_INSTALL_LIBDIR", "lib").define("BUILD_SHARED_LIBS", build_shared_libs)'
              '';

              nativeBuildInputs =
                with pkgs;
                [
                  pkg-config
                  # libprojectM (MilkDrop) is built from source by CMake, and
                  # its bindings by bindgen, which needs libclang.
                  cmake
                  rustPlatform.bindgenHook
                ]
                ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ makeWrapper ]
                ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                  rcodesign
                  icnsify
                ];
              # The command-alias regression starts its own bus, isolated from
              # the listener's desktop and running application.
              nativeCheckInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.dbus ];
              buildInputs =
                pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
                  with pkgs;
                  [
                    alsa-lib
                    libpulseaudio
                    # libprojectM links OpenGL directly and its GL loader needs
                    # X11 headers while it is built.
                    libGL
                    libx11
                  ]
                )
                ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.apple-sdk ];

              # Nix's Rust check hook deliberately points SSL_CERT_FILE at a
              # missing path. The librespot proxy regression test constructs a
              # TLS connector before asserting its CONNECT request, so give the
              # test an explicit, sandboxed trust store.
              preCheck = ''
                export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              '';

              # The GUI dlopens its Wayland, X11 and GL libraries at run time.
              postFixup =
                pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                  wrapProgram $out/bin/musify \
                    --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath runtimeLibs}
                ''
                + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
                  rcodesign sign "$out/Applications/Musify.app"
                '';

              postInstall =
                pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                  install -Dm644 packaging/applications/musify.desktop \
                    $out/share/applications/musify.desktop
                  install -Dm644 packaging/icons/musify.svg \
                    $out/share/icons/hicolor/scalable/apps/musify.svg
                  install -Dm644 contrib/omarchy/musify.json.tpl \
                    $out/share/musify/omarchy/musify.json.tpl
                  install -Dm755 contrib/omarchy/musify-theme \
                    $out/share/musify/omarchy/musify-theme
                ''
                + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
                  app="$out/Applications/Musify.app/Contents"
                  mkdir -p "$app/MacOS" "$app/Resources"
                  executable=Musify
                  identifier=io.github.msedge_projects.Musify
                  cp "$out/bin/musify" "$app/MacOS/$executable"
                  icnsify packaging/macos/icon-1024.png -o "$app/Resources/musify.icns"
                  substitute packaging/macos/Info.plist "$app/Info.plist" \
                    --replace-fail __VERSION__ "${version}" \
                    --replace-fail __BUILD__ "${pkgs.lib.head (pkgs.lib.splitString "-" version)}" \
                    --replace-fail __EXECUTABLE__ "$executable" \
                    --replace-fail __IDENTIFIER__ "$identifier"
                '';

              meta = {
                description = "Fast native Spotify client with local playback and Spotify Connect";
                homepage = "https://github.com/msedge-projects/spotifast-yt";
                license = pkgs.lib.licenses.mit;
                mainProgram = "musify";
              };
            };

        in
        {
          default = musify;
          inherit musify;
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
          musify-app = musify;
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
