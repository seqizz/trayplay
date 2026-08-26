{
  description = "trayplay - systray Jellyfin music player";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # Toolchain pinned by rust-toolchain.toml so cargo/rustc match outside Nix too.
        toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        # GTK apps need the gapps wrapper for GSettings schemas and icon themes.
        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
          # glib-compile-resources, called by build.rs for the icon bundle.
          glib
        ];

        # No openssl: reqwest will use rustls, keeping the closure smaller.
        buildInputs = with pkgs; [
          glib
          gtk4
          libadwaita
          gtk4-layer-shell
          alsa-lib
          # Symbolic transport icons come from here; wrapGAppsHook4 needs it on
          # the closure to put it in XDG_DATA_DIRS.
          adwaita-icon-theme
        ];

        # craneLib.cleanCargoSource keeps only *.rs, *.toml and Cargo.lock, which
        # drops data/ - and data/default.css is include_str!'d, the icons are
        # compiled by build.rs, and postInstall reads both. So the filter is
        # widened rather than used as-is.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "trayplay-source";
          filter = path: type:
            (craneLib.filterCargoSources path type)
            || (builtins.match ".*/data(/.*)?" path != null);
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          inherit nativeBuildInputs buildInputs;
        };

        # Separate dep-only derivation so source edits do not rebuild the world.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        trayplay = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;

          postInstall = ''
            install -Dm644 data/trayplay.svg \
              $out/share/icons/hicolor/scalable/apps/trayplay.svg
            install -Dm644 data/trayplay.desktop \
              $out/share/applications/trayplay.desktop
            install -Dm644 data/default.css $out/share/trayplay/default.css

            # Icons and fonts are compiled *into* the binary, so their notices
            # have to travel with the package rather than with any installed
            # asset - there is no installed asset to attach them to.
            install -Dm644 data/icons/ionicons/LICENSE \
              $out/share/licenses/trayplay/ionicons-LICENSE
            install -Dm644 data/icons/phosphor/LICENSE \
              $out/share/licenses/trayplay/phosphor-LICENSE
            install -Dm644 data/icons/qlementine/LICENSE \
              $out/share/licenses/trayplay/qlementine-LICENSE
            # MynaUI's terms (MIT, no attribution required) were never in hand as
            # upstream text, so its SOURCES.md is the record - see CLAUDE.md.
            install -Dm644 data/icons/mynaui/SOURCES.md \
              $out/share/licenses/trayplay/mynaui-NOTICE.md
            # Guarded rather than assumed: data/fonts is a drop-in directory and
            # may legitimately be empty, in which case no font is embedded and
            # there is nothing to license.
            if [ -f data/fonts/OFL.txt ]; then
              install -Dm644 data/fonts/OFL.txt \
                $out/share/licenses/trayplay/fonts-OFL.txt
              install -Dm644 data/fonts/README.md \
                $out/share/licenses/trayplay/fonts-NOTICE.md
            fi
          '';

          meta = with pkgs.lib; {
            description = "Systray Jellyfin music player with MPRIS support";
            mainProgram = "trayplay";
            platforms = platforms.linux;
          };
        });

        # Version the release artifacts are named after. Read from Cargo.toml so
        # the tag, the tarball and `trayplay --version` cannot disagree.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

        # The whole install tree in one archive, which is what a release
        # attachment carries. Contents are exactly `trayplay`'s $out, so an
        # unpacked tarball has bin/, share/applications, share/licenses and the
        # rest in the places `prebuilt` expects them.
        releaseTarball = pkgs.runCommand
          "trayplay-${version}-${system}.tar.gz"
          { nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ]; }
          ''
            # Dereferenced: $out/bin/trayplay is a wrapper script next to
            # .trayplay-wrapped, and both are real files, but share/ can hold
            # symlinks into the store that would be dangling once unpacked
            # elsewhere.
            tar --dereference --owner=0 --group=0 --numeric-owner \
              --sort=name --mtime='@1' \
              -czf $out -C ${trayplay} .
          '';

        # Where releases live. Forgejo uses the same shape as GitHub -
        # <host>/<owner>/<repo>/releases/download/<tag> - so a mirror works with
        # nothing but a host change, and the tag is derived from `version` above
        # rather than written out again.
        releaseBaseUrl = "https://git.gurkan.in/gurkan/trayplay/releases/download/v${version}";

        # Artifact hashes for `.#prebuilt`, keyed by version and then system.
        #
        # Only the hash is pinned by hand: which release to install follows
        # `Cargo.toml`, so a version bump points at its own release with no
        # second edit. A hash cannot follow anything - Nix needs it at eval time
        # to fetch at all, and asking a forge for "latest" would be network
        # access during evaluation, which pure flake eval forbids. So bumping the
        # version *before* its release exists leaves no entry here, and that is
        # the honest outcome: `.#prebuilt` fails with the message below instead
        # of fetching a tarball nobody has verified.
        #
        # Inline rather than a separate pin file on purpose: a new file has to be
        # `git add`ed before a flake can even see it (Nix copies tracked files
        # only), which is a confusing failure for something that looks like data.
        # The release workflow prints a ready-made entry in the release notes, so
        # updating it is a copy-paste and a commit. Old entries can stay - they
        # cost nothing and document what was released.
        prebuiltHashes = {
          "0.2.0" = {
            x86_64-linux = "sha256-YfEDOvpfKm1iJjsp0Yglos0AGV2U1CNJr90zDxu4+AQ=";
          };
          "0.2.1" = {
            x86_64-linux = "sha256-TDPEOfrsuTZDT04ExwvhE7GMpm/ceK++4/mZ0MTQ80E=";
          };
        };

        # Installs the binary from a Forgejo release instead of building it.
        #
        # Deliberate trade-off, chosen by the operator: it skips a ten-minute
        # compile, at the cost of being sensitive to nixpkgs drift. The binary was
        # linked against the *builder's* GTK stack, so autoPatchelfHook rewrites
        # its RPATH against the consumer's - which works as long as the sonames
        # still match. A major GTK/glib bump on either side is expected to break
        # this with a "could not satisfy dependency" from autoPatchelf, and the
        # answer then is to build from source (`nix build .#trayplay`) until a new
        # release is cut. It is also NixOS-only: nothing here helps a Debian box.
        pinnedHash = prebuiltHashes.${version}.${system} or "";

        # An absent or empty hash means this version was never released for this
        # system. Reported by a derivation that fails when *built* rather than by
        # a throw while evaluating: a throw would make `nix flake check` fail on a
        # flake that is perfectly fine, just not released yet.
        prebuilt = if pinnedHash == "" then
          pkgs.runCommand "trayplay-bin-unavailable" { } ''
            echo "No prebuilt trayplay ${version} for ${system}: no hash for that" >&2
            echo "version in flake.nix's prebuiltHashes - either it has not been" >&2
            echo "released yet, or the entry is missing. Build from source" >&2
            echo "instead:  nix build .#trayplay" >&2
            exit 1
          ''
        else pkgs.stdenv.mkDerivation {
          pname = "trayplay-bin";
          inherit version;

          src = pkgs.fetchurl {
            url = "${releaseBaseUrl}/trayplay-${version}-${system}.tar.gz";
            hash = pinnedHash;
          };

          sourceRoot = ".";

          nativeBuildInputs = with pkgs; [ autoPatchelfHook wrapGAppsHook4 ];
          # The same set the source build links against: autoPatchelf resolves
          # against these, and wrapGAppsHook4 needs them for XDG_DATA_DIRS.
          inherit buildInputs;

          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r ./* $out/
            runHook postInstall
          '';

          # The tarball carries the builder's own wrapper script, whose paths point
          # into a store this machine may not have. Dropped so wrapGAppsHook4 can
          # write a fresh one around the real binary.
          preFixup = ''
            if [ -e $out/bin/.trayplay-wrapped ]; then
              mv $out/bin/.trayplay-wrapped $out/bin/trayplay
            fi
          '';

          meta = trayplay.meta // {
            description = "${trayplay.meta.description} (prebuilt release binary)";
          };
        };
      in
      {
        packages = {
          default = trayplay;
          inherit trayplay prebuilt releaseTarball;
        };

        apps.default = flake-utils.lib.mkApp { drv = trayplay; };

        # craneLib.devShell builds nativeBuildInputs itself from the toolchain and
        # `packages`, so build tools must go in `packages` or their setup hooks
        # (notably pkg-config's PKG_CONFIG_PATH) never run.
        devShells.default = craneLib.devShell {
          inherit (commonArgs) buildInputs;

          packages = with pkgs; [
            pkg-config
            # glib-compile-resources for build.rs.
            glib
            # Not needed for the tray any more (trayplay docks into XEmbed
            # directly on X11 now, see src/tray/xembed.rs), but still useful
            # for testing the SNI path under XWayland without a full Wayland
            # session.
            snixembed
            playerctl
            d-spy
          ];

          # wrapGAppsHook4 only applies at install time, so an unwrapped
          # `cargo run` needs the schema and icon lookup paths by hand.
          shellHook = ''
            export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.adwaita-icon-theme}/share:$XDG_DATA_DIRS"
          '';
        };

        # No rustfmt check on purpose. The layout here is hand-written - comment
        # wrapping and argument breaks are chosen to read a certain way - and
        # rustfmt disagrees with most of it. Enforcing it would mean one
        # mechanical reflow of the whole tree and then living with its opinions
        # about every comment thereafter. Clippy *is* enforced, warnings denied,
        # because that catches mistakes rather than style.
        checks = {
          inherit trayplay;
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });
        };
      });
}
