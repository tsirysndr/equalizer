Name:           equalizer
Version:        0.1.0
Release:        1%{?dist}
Summary:        A real-time terminal equalizer for raw PCM pipes

License:        GPL-2.0-or-later
URL:            https://github.com/tsirysndr/equalizer

BuildArch:      x86_64

Requires: glibc, alsa-lib

%description
equalizer reads raw PCM from stdin, a FIFO, or a unix socket, runs it
through the Rockbox DSP (10-band EQ, bass/treble shelves, resampling) and
plays the result on your sound card — while a Synthwave '84 ratatui
interface lets you tweak the bands live. Settings persist to a TOML file
in Rockbox's own eq_band_settings format.

%prep
# Nothing to prep — the binary is prebuilt.

%build
# Nothing to build — the binary is prebuilt.

%install
mkdir -p %{buildroot}/usr/local/bin
cp -r %{_sourcedir}/amd64/usr %{buildroot}/

%files
/usr/local/bin/equalizer

%post
if [ "$1" -eq 1 ]; then
    echo "equalizer: installed. Try:  ffmpeg -i track.flac -f s16le -ac 2 -ar 44100 - | equalizer"
fi
