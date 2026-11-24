# Changelog

## 0.1.0

First implementation release for KEO CLS.EEDI 1.2.0.

- Encode and validate protocol messages, track per-device controls and recover pending operations after a restart.
- Read signed meter values and aggregate active power across a provisioned group.
- Connect MQTT 5 brokers and EEBUS equipment through optional features, including SHIP pairing confirmation.
- Inspect captured messages, meter readings and commissioning status with the `cls-eedi` CLI.

## 0.0.1

Reserved the crate name.
