# front

**Fancy Radar ObservatioN Tool** - a live European weather radar map that runs in your terminal, because sometimes you want to watch a storm roll in without leaving tmux.

Uses braille characters to render radar reflectivity, weather warnings, and surface observations at surprisingly decent resolution. Data comes from MeteoGate, MeteoAlarm, and EUMETNET.

## Install

Needs Rust (1.70+). Quickest way:

```bash
cargo install --git https://github.com/mpevec/front
```

Or clone and build locally:

```bash
git clone https://github.com/mpevec/front
cd front
cargo build --release
./target/release/front
```

Without MQTT support (smaller binary):

```bash
cargo build --release --no-default-features
```

## Usage

```bash
front                                        # auto-detect location via GeoClue
front --lat 46.0 --lon 14.5 --zoom 6.0      # start at a specific location
front --no-location                          # skip location lookup, defaults to Europe
front --clear-cache                          # wipe cached tiles and restart
```

Config is at `~/.config/front/config.toml` and gets created on first run. Tiles are cached under `~/.cache/front/`.

## Requirements

- Linux (GeoClue2 D-Bus is used for location, use `--no-location` to skip it)
- A terminal with braille character support (most modern ones are fine)
- An internet connection, the radar data won't fetch itself

## Data sources

Big thanks to the people behind these:

| Source | What it provides |
|---|---|
| [MeteoGate](https://meteogate.eu) | Radar tiles and surface observations |
| [MeteoAlarm](https://meteoalarm.org) | Weather warning polygons |
| [EUMETNET](https://eumetnet.eu) | Surface observation network |
| [Natural Earth](https://naturalearthdata.com) | Country and region borders |

## License

MIT, see [LICENSE](LICENSE). Use it, fork it, ship it, whatever. Just keep the copyright notice so people know where it came from. And don't blame me if the forecast is wrong, meteorology is hard.
