Null Notes @VERSION@ for Linux (x86_64)
=======================================

Run it straight from this folder:

    ./null-notes

Or install it (adds it to your app menu and handles nullnotes:// links):

    ./install.sh                 just for you, into ~/.local (no sudo)
    sudo ./install.sh --system   for everyone, into /usr/local

To remove it: ./uninstall.sh (or sudo ./uninstall.sh --system).
Your vaults are folders you chose; they're never touched.

Needs: glibc 2.35 or newer, and WebKitGTK 4.1 (most desktops already have it):
    Ubuntu/Debian:   sudo apt install libwebkit2gtk-4.1-0
    Fedora:          sudo dnf install webkit2gtk4.1
    openSUSE:        sudo zypper install libwebkit2gtk-4_1-0
    Arch:            sudo pacman -S webkit2gtk-4.1

More: https://github.com/Null0Grid/null-notes
