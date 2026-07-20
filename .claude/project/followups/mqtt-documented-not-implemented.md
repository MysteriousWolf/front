---
id: mqtt-documented-not-implemented
title: MQTT live-update support documented but not implemented
created: "2026-07-20"
origin: |
    /refresh-signals scan
kind: finding
severity: risk
review_by: "2026-09-18"
status: open
file: CLAUDE.md
---

CLAUDE.md's Commands section claims the default 'mqtt' feature adds MQTT live-update support for MeteoAlarm. Verified: rumqttc is an optional dep (Cargo.toml:21) and the feature exists (Cargo.toml:56), and config.rs carries a meteoalarm.mqtt_broker setting (config.rs:129), but no code under src/ references rumqttc or cfg(feature = "mqtt"). The feature is manifest- and config-only. Either wire it up in meteoalarm.rs or correct the doc claim.
