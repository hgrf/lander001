## rustyface

### Prerequisites

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install ldproxy
cargo install espflash
```

### Pinout

ESP32-C3 Super Mini to ST7789 240x240 TFT Display:

| Function | ESP32-C3 Pin | Display Pin | Description |
|----------|--------------|-------------|-------------|
| SCK      | GPIO4        | SCL         | SPI Clock |
| MOSI     | GPIO6        | SDA         | SPI Data Out |
| DC       | GPIO1        | DC          | Data/Command |
| RST      | GPIO0        | RST         | Reset |
