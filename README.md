# OpenXplorer

**OpenXplorer RANDY** es una edición de [OpenXplorer de AKolenda](https://github.com/AKolenda/openxplorer), basada en [v2.0.18](https://github.com/AKolenda/openxplorer/releases/tag/v2.0.18) y adaptada para Zorin OS. Los enlaces de descargas del proyecto original se identifican a continuación.

## Instalar en Zorin OS

Descarga el [Flatpak de OpenXplorer RANDY](https://github.com/Maxdesigna7x/OpenXplorer/releases/download/v2.0.18-randy.1/OpenXplorer-RANDY-2.0.18.flatpak) para Linux x86-64. Ábrelo en la tienda de software o ejecuta lo siguiente desde la carpeta de descarga.

```sh
flatpak install --user ./OpenXplorer-RANDY-2.0.18.flatpak
```

Abre **OpenXplorer RANDY** desde el menú de aplicaciones. El paquete usa el runtime GNOME 51 de Flathub. En la [publicación de GitHub](https://github.com/Maxdesigna7x/OpenXplorer/releases/tag/v2.0.18-randy.1) también están el código fuente correspondiente y la suma SHA-256.

Para generar el paquete desde el código, usa `tools/build-randy-flatpak.sh` en un entorno de compilación con Rust, las bibliotecas de [native](native/README.md) y el SDK GNOME 51.

## Cambios de OpenXplorer RANDY

Modificaciones de esta edición registradas el 16 de noviembre de 2026.

- Iconos de carpetas y tipos de archivo del tema de escritorio, con prioridad para los iconos personalizados que guarda GNOME Archivos.
- Espaciado y relleno de la cuadrícula, radio de las esquinas, selección azul opcional y paletas independientes para los modos claro y oscuro. Cada color se edita en HSV.
- Inicio en **This PC** y activación de ventanas al pasar por la barra de Zorin durante un arrastre en X11.
- Uso de las asociaciones del escritorio y del selector del sistema en Flatpak, miniaturas compartidas con GNOME Archivos y acceso al Google Drive de las Cuentas en línea de GNOME desde **Network** y la sección **Network locations** de **This PC**.
- **Local Disk** muestra la capacidad real del disco raíz. En el Flatpak abre la raíz de Zorin en GNOME Archivos, que tiene acceso a esa vista del sistema.
- Flatpak separado con identificador `io.winspace.Development.Native`.

La edición conserva [AGPL-3.0-only](LICENSE) para las modificaciones y los avisos de [atribución](NOTICE), [licencias de terceros](THIRD_PARTY_NOTICES.md) y [Winspace](licenses/) del proyecto original. El código de la aplicación y las instrucciones de compilación están en [native](native/README.md).

## Proyecto original

A Windows File Explorer-inspired file manager for Linux, **developed for Zorin OS first and foremost**. Browse local folders, SMB shares and SFTP, FTP, WebDAV and NFS locations with tabs, split panes, a folder tree, Details, Compact, List and icon views with thumbnails, clickable paths, pinned folders, search, undoable file operations and light/dark themes.

<a href="https://github.com/AKolenda/openxplorer/releases/latest"><img src="https://img.shields.io/github/v/release/AKolenda/openxplorer?style=for-the-badge&label=Download&labelColor=0067c0&color=004e8c&logo=linux&logoColor=white" alt="Download OpenXplorer" height="40"></a>

**[Releases](https://github.com/AKolenda/openxplorer/releases)** · **[Website](https://openxplorer.app)** · **[Installation](docs/installation.md)** · **[Documentation](docs/introduction.md)**

![OpenXplorer browsing fictional sample files](apps/web/public/assets/screenshots/explorer-light.png)

![Two panes side by side showing the same fictional folder](apps/web/public/assets/screenshots/split-view.png)

*The native GTK 4 app, captured in an isolated session with fictional files: the Details view, and split panes. Network examples in these pictures do not establish live SMB compatibility.*

## Install

Zorin OS is the primary target for its desktop experience and integration; Ubuntu and Debian are secondary compatibility targets and require compatible system packages.

Version **2.0.18** is a native GTK 4 application written in Rust (`native/`). It keeps the same look, settings, pins and saved passwords as 1.x; see the [changelog](CHANGELOG.md) for what 2.0.18 adds. Get it from [GitHub Releases](https://github.com/AKolenda/openxplorer/releases):

- **Zorin OS 18, Ubuntu 24.04 and newer, Debian 13:** `openxplorer_2.0.18_all.deb` (x86-64). OpenXplorer 1.1.x offers it in **Check for updates**.
- **Fedora, openSUSE Tumbleweed, Arch Linux:** the `.rpm` or `.pkg.tar.zst` of the release, when the release lists one.
- **Any distribution with Flatpak**, including Debian 12 and others with GTK older than 4.14: `io.winspace.Development.flatpak`.

Follow the [installation guide](docs/installation.md), and finish file operations and run `openxplorer --quit` before upgrading. See the [changelog](CHANGELOG.md), the [known gaps and backlog](native/BACKLOG.md), the [packaging guide](native/packaging/README.md) and the [release checklist](docs/RELEASE-CHECKLIST.md).

## Develop

The app lives in `native/` (Rust, GTK 4, GIO/GVfs); see [native/README.md](native/README.md). It needs Rust 1.92 or newer and the GTK 4.14+, SQLite, libsoup 3 and GStreamer development packages (`libgtk-4-dev libsqlite3-dev libsoup-3.0-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev` on Ubuntu and Debian):

```sh
cargo build --release --locked --manifest-path native/Cargo.toml
python3 native/tools/check.py
```

No package ships Python; the persistent SMB mount helper is a Rust program too. The Next.js website lives in `apps/web/`. The installed app does not depend on Node or pnpm.

**Retired:** the Python/GTK 3/WebKitGTK app of OpenXplorer 1.x is no longer in the tree, released or maintained. Its last release is tag [v1.1.4](https://github.com/AKolenda/openxplorer/tree/v1.1.4); its final sources, which `native/parity/` cites as the behavioural specification, are `desktop/` at tag [v2.0.0](https://github.com/AKolenda/openxplorer/tree/v2.0.0/desktop).

For the website, use Node.js 22.13+ and pnpm 10.34.5:

```sh
pnpm install --frozen-lockfile
pnpm dev
```

See [native setup](native/README.md), [website setup](apps/web/README.md) and the [development guide](docs/development.md) for prerequisites, builds and checks.

## Contribute

Read [CONTRIBUTING.md](CONTRIBUTING.md) and submit a pull request from a feature branch. `main` requires PRs, including for administrators. Report vulnerabilities using [SECURITY.md](SECURITY.md).

## License

Project changes, website and project-authored documentation are **[AGPL-3.0-only](LICENSE)**, with documented file-level exceptions. Preserve the upstream Winspace [MIT notice](licenses/Winspace-MIT.txt), [NOTICE](NOTICE) and [third-party notices](THIRD_PARTY_NOTICES.md).

Independent project; not affiliated with Microsoft, Zorin, Canonical, Debian or Vercel. No warranty.
