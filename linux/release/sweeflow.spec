# The RPM (Fedora, openSUSE, ...), built by build.sh from the release tarball. Requires on the libraries
# come from rpmbuild reading the binary (libva.so.2, libxkbcommon.so.0, ...), so they hold on any of them.
Name:           sweeflow
Version:        %{version}
Release:        1
Summary:        Mac apps as windows on a Linux desktop
License:        MIT AND LGPL-2.1-or-later
URL:            https://github.com/herunan/sweeflow
Source0:        sweeflow-%{version}-x86_64.tar.gz
ExclusiveArch:  x86_64
Requires:       systemd
Recommends:     (pipewire-utils or pulseaudio-utils)
Recommends:     libnotify

# A prebuilt binary: nothing to compile, and no debug package.
%global debug_package %{nil}
%global __strip /bin/true

%description
Shows the apps of a Mac as windows on this Linux desktop, over the network.
Open Sweeflow from the app menu (or run `sweeflow install`) once as your user:
it starts with your desktop from then on, and offers to link a Mac.

%prep
%setup -q -n sweeflow-%{version}

%install
install -Dm755 sweeflow %{buildroot}/usr/bin/sweeflow
install -Dm644 sweeflow.desktop %{buildroot}/usr/share/applications/sweeflow.desktop
install -Dm644 sweeflow.svg %{buildroot}/usr/share/icons/hicolor/scalable/apps/sweeflow.svg
install -Dm644 THIRD-PARTY-LICENSES.html %{buildroot}/usr/share/doc/sweeflow/THIRD-PARTY-LICENSES.html
install -Dm644 LICENSE %{buildroot}/usr/share/licenses/sweeflow/LICENSE

%files
/usr/bin/sweeflow
/usr/share/applications/sweeflow.desktop
/usr/share/icons/hicolor/scalable/apps/sweeflow.svg
/usr/share/doc/sweeflow/THIRD-PARTY-LICENSES.html
/usr/share/licenses/sweeflow/LICENSE
