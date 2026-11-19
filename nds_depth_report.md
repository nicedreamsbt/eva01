# nicedreamswap depth and coverage, every p0 mint

Measured 134 p0 mints against USDC on the VPS, selling the token for USDC at $1k, $10k, $50k, $100k and $250k and buying it with USDC.
Impact is in basis points against the mid price from a $100 Jupiter quote. Each cell is **NDS / Jupiter (all venues)**.
These NDS numbers are from the daemon **before splitting** (one pool per hop); splitting adds about 3 bps at $100k and 8 bps at $250k on SOL/USDC.

- Mints Jupiter can route at all: 92 of 134 (the other 42 are PT, ns and st tokens and similar with no Jupiter route).
- Mints NDS can route at $10k: **36 of 92**.
- Gaps are mostly venues NDS does not read yet: ZeroFi, BisonFi, Scorch, Kipseli, Raydium AMM v4, Sanctum and stake pools, Manifest, Meteora DAMM, Raydium CP, GoonFi.
- The largest size within 50 bps is the biggest tested size whose impact is at or under 50 bps; `<$1k` means none of the tested sizes.

| Mint | Jupiter liquidity | NDS routes | $10k sell | $100k sell | $250k sell | max size ≤50 bps | Missing |
| --- | --- | --- | --- | --- | --- | --- | --- |
| SOL | 781.84M | yes | 5 / -2 | 15 / 5 | 29 / -0 | $250k / $250k |  |
| BNSOL | 760.44M | NO | – / 34 | – / 24 | – / 27 | <$1k / $250k | NO: Kipseli, VaultLiquidUnstake, ZeroFi |
| JitoSOL | 740.60M | yes | 7 / -0 | 13 / -2 | 28 / 4 | $250k / $250k |  |
| JupSOL | 376.26M | NO | – / -6 | – / 6 | – / 27 | $1k / $250k | NO: BisonFi, Manifest, Sanctum Prop S |
| fwdSOL | 289.35M | NO | – / 20 | – / 20 | – / 24 | <$1k / $250k | NO: Kipseli, Sanctum, Sanctum Infinity |
| dSOL | 205.13M | NO | – / 18 | – / 22 | – / 28 | <$1k / $250k | NO: Kipseli, Sanctum Infinity, ZeroFi |
| mSOL | 162.37M | yes | 8 / 3 | 18 / 7 | 35 / 11 | $250k / $250k |  |
| dfdvSOL | 160.25M | NO | – / 19 | – / 24 | – / 29 | <$1k / $250k | NO: Kipseli, Sanctum, Sanctum Infinity |
| INF | 138.36M | yes | – / 7 | – / 11 | – / 12 | $1k / $250k |  |
| JSOL | 94.13M | yes | 22 / 6 | – / 6 | – / 12 | $10k / $250k |  |
| bbSOL | 89.19M | NO | – / 24 | – / 25 | – / 28 | <$1k / $250k | NO: Byreal, Kipseli, Sanctum, ZeroFi |
| hSOL | 73.58M | yes | 139 / 3 | 175 / 10 | – / 12 | <$1k / $250k |  |
| bSOL | 64.44M | yes | 6 / 5 | 17 / 7 | 36 / 11 | $250k / $250k |  |
| STKESOL | 40.69M | NO | – / 20 | – / 21 | – / 24 | <$1k / $250k | NO: Kipseli, Sanctum, VaultLiquidUnstake |
| USDG | 33.32M | NO | – / 0 | – / 0 | – / 0 | <$1k / $250k | NO: Manifest, Scorch, ZeroFi |
| PUMP | 30.04M | NO | – / 5 | – / 12 | – / 37 | <$1k / $250k | NO: GoonFi V2, Kipseli, Scorch, ZeroFi |
| WBTC | 29.42M | yes | 14 / 3 | 67 / 8 | 167 / 13 | $50k / $250k |  |
| PYUSD | 29.33M | NO | – / -2 | – / -2 | – / -2 | <$1k / $250k | NO: BisonFi, Manifest, Scorch |
| USDT | 28.03M | yes | 0 / -0 | 0 / 0 | 1 / 0 | $250k / $250k |  |
| USD1 | 22.53M | yes | 1 / 1 | 1 / 1 | 1 / 1 | $250k / $250k |  |
| cbBTC | 18.83M | yes | 7 / 5 | 57 / 1 | 102 / 5 | $50k / $250k |  |
| JupUSD | 17.87M | yes | 0 / 0 | 1 / 0 | 1 / 0 | $250k / $250k |  |
| ETH | 15.92M | yes | 17 / 1 | 40 / 6 | 79 / 14 | $100k / $250k |  |
| hyloSOL | 13.93M | NO | – / -1 | – / 5 | – / 8 | <$1k / $250k | NO: Hylo Exchange, Manifest, ZeroFi |
| rkuSOL | 13.53M | NO | – / 31 | – / 31 | – / 34 | <$1k / $250k | NO: Sanctum Infinity, Scorch, ZeroFi |
| syrupUSDC | 7.87M | NO | – / 2 | – / 2 | – / 2 | <$1k / $250k | NO: BisonFi, Scorch |
| USDe | 7.75M | NO | – / 2 | – / 3 | – / 3 | <$1k / $250k | NO: BisonFi, Manifest, Scorch, ZeroFi |
| CASH | 7.54M | NO | – / 0 | – / 0 | – / 0 | <$1k / $250k | NO: BisonFi, Manifest, ZeroFi |
| LST | 6.68M | NO | – / 7 | – / 18 | – / 17 | <$1k / $250k | NO: BisonFi, Meteora, Scorch |
| Bonk | 5.70M | yes | 231 / 57 | – / 3133 | – / 6109 | $1k / $1k |  |
| bonkSOL | 5.50M | NO | – / 21 | – / 16 | – / 20 | <$1k / $250k | NO: Sanctum Infinity, ZeroFi |
| BulkSOL | 5.23M | NO | – / 33 | – / 27 | – / 25 | <$1k / $250k | NO: BisonFi, VaultLiquidUnstake |
| JUP | 5.13M | yes | 54 / 5 | 196 / 16 | 457 / 130 | $1k / $100k |  |
| HYPE | 5.00M | yes | 14 / -5 | 99 / – | 214 / 21 | $50k / $250k |  |
| ZEC | 4.32M | yes | 96 / -8 | 831 / 14 | – / 21 | $1k / $250k |  |
| $WIF | 4.15M | NO | – / 96 | – / 511 | – / 1194 | <$1k / <$1k | NO: Raydium, Scorch |
| JLP | 3.99M | yes | 64 / 3 | 609 / 13 | – / 22 | $1k / $250k |  |
| MET | 3.87M | yes | 34 / 24 | 328 / 156 | 819 / 358 | $10k / $10k |  |
| stSOL | 3.25M | yes | 2031 / 25 | – / 276 | – / 597 | <$1k / $10k |  |
| corvusSOL | 2.70M | NO | – / 20 | – / 22 | – / 30 | <$1k / $250k | NO: Sanctum, Sanctum Infinity, Scorch, ZeroFi |
| YIELD | 1.98M | NO | – / 36 | – / 30 | – / 36 | <$1k / $250k | NO: VaultLiquidUnstake, ZeroFi |
| hyUSD | 1.94M | NO | – / 2 | – / 2 | – / 2 | <$1k / $250k | NO: Hylo Exchange, ZeroFi |
| USDY | 1.90M | NO | – / 32 | – / 36 | – / 41 | <$1k / $250k | NO: BisonFi, JupLend AMM |
| lanternSOL | 1.86M | NO | – / 20 | – / 26 | – / 28 | <$1k / $250k | NO: Kipseli, Sanctum Infinity, ZeroFi |
| picoSOL | 1.35M | NO | – / 21 | – / 21 | – / 23 | <$1k / $250k | NO: Kipseli, Sanctum Infinity, ZeroFi |
| USDS | 1.12M | yes | – / 2 | – / 4 | – / 8 | $1k / $250k |  |
| ORE | 1.02M | yes | 198 / 124 | 1519 / 925 | 2663 / 1901 | <$1k / <$1k |  |
| jucySOL | 0.91M | NO | – / 19 | – / 18 | – / 23 | <$1k / $250k | NO: Deriverse, Sanctum Infinity, ZeroFi |
| laineSOL | 0.89M | NO | – / 41 | – / 29 | – / 31 | <$1k / $250k | NO: Kipseli, VaultLiquidUnstake, ZeroFi |
| rkSOL | 0.81M | NO | – / 21 | – / 22 | – / 24 | <$1k / $250k | NO: BisonFi, Deriverse, Sanctum Infinity |
| dzSOL | 0.78M | NO | – / 45 | – / 30 | – / 32 | <$1k / $250k | NO: Kipseli, Sanctum, VaultLiquidUnstake, ZeroFi |
| 2Z | 0.73M | NO | – / 77 | – / 1160 | – / 3212 | <$1k / $1k | NO: Scorch, ZeroFi |
| SANC | 0.66M | yes | 564 / 388 | – / 2575 | – / 5844 | <$1k / <$1k |  |
| compassSOL | 0.54M | NO | – / 19 | – / 25 | – / 30 | <$1k / $250k | NO: Kipseli, Sanctum Infinity, ZeroFi |
| EURC | 0.53M | yes | 8 / 8 | – / – | – / – | $10k / $10k |  |
| raSOL | 0.52M | NO | – / 20 | – / 20 | – / 24 | <$1k / $250k | NO: Sanctum, Sanctum Infinity, ZeroFi |
| NOS | 0.43M | yes | 269 / 191 | 1685 / 1255 | – / 2647 | <$1k / <$1k |  |
| PYTH | 0.37M | yes | 133 / 84 | 892 / 685 | – / 3009 | $1k / $1k |  |
| JTO | 0.32M | yes | – / 155 | – / 1280 | – / 2697 | <$1k / <$1k |  |
| xBTC | 0.23M | NO | – / 3 | – / 64 | – / 1520 | <$1k / $50k | NO: BisonFi, GoonFi V2, Manifest, ZeroFi |
| RENDER | 0.21M | yes | 159 / 120 | 1085 / 754 | – / 2834 | <$1k / <$1k |  |
| ORCA | 0.19M | yes | 97 / 79 | – / 652 | – / 1886 | $1k / $1k |  |
| HNT | 0.17M | NO | – / 650 | – / 3932 | – / 6171 | <$1k / <$1k | NO: Raydium, Raydium CP, Scorch, ZeroFi |
| cgntSOL | 0.15M | NO | – / 42 | – / 26 | – / 81 | <$1k / $100k | NO: Kipseli, VaultLiquidUnstake, ZeroFi |
| W | 0.14M | yes | – / 568 | – / 4472 | – / 7095 | <$1k / <$1k |  |
| boden | 0.12M | NO | – / 830 | – / 4603 | – / 6797 | <$1k / <$1k | NO: Raydium, ZeroFi |
| MOTHER | 0.08M | NO | – / 1177 | – / 5604 | – / 7605 | <$1k / <$1k | NO: BisonFi, Raydium, ZeroFi |
| GUAC | 0.08M | NO | – / 1256 | – / 5796 | – / 7746 | <$1k / <$1k | NO: Raydium, Scorch, ZeroFi |
| WEN | 0.07M | yes | – / 2574 | – / 8196 | – / 9066 | <$1k / <$1k |  |
| Neiro | 0.06M | NO | – / 1403 | – / 6119 | – / 7972 | <$1k / <$1k | NO: Raydium, ZeroFi |
| zBTC | 0.05M | yes | – / 42 | – / – | – / – | <$1k / $10k |  |
| AAVE | 0.05M | yes | – / 34 | – / – | – / – | <$1k / $10k |  |
| ISC | 0.05M | NO | – / 10 | – / 4517 | – / 7717 | <$1k / $10k | NO: BisonFi, OpenBook V2, ZeroFi |
| UXD | 0.05M | NO | – / 4 | – / 86 | – / 5396 | <$1k / $50k | NO: BisonFi, Kipseli, Mercurial, Meteora, Saber, Scorch |
| DITH | 0.04M | NO | – / 2042 | – / 7076 | – / 8577 | <$1k / <$1k | NO: Meteora, Raydium, ZeroFi |
| DRIFT | 0.03M | NO | – / – | – / – | – / – | <$1k / <$1k | NO: no data |
| META | 0.03M | NO | – / 2720 | – / 7737 | – / 8947 | <$1k / <$1k | NO: BisonFi, Kipseli, Meteora, Scorch, ZeroFi |
| TNSR | 0.03M | yes | 1045 / 1045 | – / – | – / – | <$1k / <$1k |  |
| RLB | 0.03M | no Jupiter route | | | | | |
| TBTC | 0.02M | NO | – / 21 | – / 8126 | – / 9246 | <$1k / $10k | NO: BisonFi, GoonFi V2, Manifest, Meteora, Scorch |
| SNS | 0.02M | no Jupiter route | | | | | |
| PRCL | 0.02M | NO | – / – | – / – | – / – | <$1k / <$1k | NO: no data |
| STEP | 0.01M | NO | – / 4495 | – / 8895 | – / 9526 | <$1k / <$1k | NO: Byreal, Raydium, ZeroFi |
| SHDW | 0.01M | yes | – / 343 | – / 2285 | – / 4223 | <$1k / <$1k |  |
| MNDE | 0.01M | yes | – / 862 | – / 7637 | – / 9154 | <$1k / <$1k |  |
| SAMO | 0.01M | NO | – / 1298 | – / 6042 | – / 7919 | <$1k / <$1k | NO: Orca V2, Raydium, ZeroFi |
| sUSD | 0.01M | no Jupiter route | | | | | |
| HONEY | 0.01M | NO | – / 1765 | – / 8407 | – / 9383 | <$1k / <$1k | NO: BisonFi, Meteora DAMM v2, Raydium, Raydium CP, ZeroFi |
| DUST | 0.01M | NO | – / 8347 | – / 9804 | – / 9921 | <$1k / <$1k | NO: Raydium, ZeroFi |
| USD* | 0.00M | NO | – / 5 | – / 5 | – / 5 | <$1k / $250k | NO: Perena Star V2, ZeroFi |
| BLZE | 0.00M | NO | – / 6454 | – / 9529 | – / 9814 | <$1k / <$1k | NO: BisonFi, Meteora, ZeroFi |
| OPOS | 0.00M | NO | – / – | – / – | – / – | <$1k / <$1k | NO: no data |
| KIN | 0.00M | NO | – / 3882 | – / 8600 | – / 9396 | <$1k / <$1k | NO: Orca V2, Raydium, ZeroFi |
| MOBILE | 0.00M | NO | – / 726 | – / 3963 | – / – | <$1k / <$1k | NO: Helium Network, Raydium, Raydium CP, Scorch, ZeroFi |
| LFG | 0.00M | no Jupiter route | | | | | |
| PT-hyUSD | 0.00M | no Jupiter route | | | | | |
| ezSOL | 0.00M | no Jupiter route | | | | | |
| ORE | 0.00M | no Jupiter route | | | | | |
| PT-bulkSOL | 0.00M | no Jupiter route | | | | | |
| zenBTC | 0.00M | no Jupiter route | | | | | |
| wstETH | 0.00M | NO | – / 759 | – / 9707 | – / 9881 | <$1k / <$1k | NO: ZeroFi |
| PT-hyUSD | 0.00M | no Jupiter route | | | | | |
| PT-bulkSOL | 0.00M | no Jupiter route | | | | | |
| PT-bulkSOL | 0.00M | no Jupiter route | | | | | |
| PT-hyloSOL | 0.00M | no Jupiter route | | | | | |
| PT-hyloSOL | 0.00M | no Jupiter route | | | | | |
| PT-hyloSOL | 0.00M | no Jupiter route | | | | | |
| PT-rkuSOL | 0.00M | no Jupiter route | | | | | |
| nsDAWN | 0.00M | no Jupiter route | | | | | |
| nsHELIUS | 0.00M | no Jupiter route | | | | | |
| nsJUPITER | 0.00M | no Jupiter route | | | | | |
| nsKILN | 0.00M | no Jupiter route | | | | | |
| nsNANSEN | 0.00M | no Jupiter route | | | | | |
| nsSHIFT | 0.00M | no Jupiter route | | | | | |
| st3N7s9zX | 0.00M | no Jupiter route | | | | | |
| st3ZUQekq | 0.00M | no Jupiter route | | | | | |
| st3xjfK9C | 0.00M | no Jupiter route | | | | | |
| st4m1Pbxz | 0.00M | no Jupiter route | | | | | |
| st7K8DVxt | 0.00M | no Jupiter route | | | | | |
| st8Pep3Gm | 0.00M | no Jupiter route | | | | | |
| stCcaHc2L | 0.00M | no Jupiter route | | | | | |
| stFKsC411 | 0.00M | no Jupiter route | | | | | |
| stHvsD9L5 | 0.00M | no Jupiter route | | | | | |
| stJ2nUHEA | 0.00M | no Jupiter route | | | | | |
| stSBLZib4 | 0.00M | no Jupiter route | | | | | |
| stSimpj3K | 0.00M | no Jupiter route | | | | | |
| stabc1zP7 | 0.00M | no Jupiter route | | | | | |
| stgaToR24 | 0.00M | no Jupiter route | | | | | |
| stmnvkHm4 | 0.00M | no Jupiter route | | | | | |
| stmrgn2vs | 0.00M | no Jupiter route | | | | | |
| stmrgn4t2 | 0.00M | no Jupiter route | | | | | |
| stnfGcSJk | 0.00M | no Jupiter route | | | | | |
| stvoteRnv | 0.00M | no Jupiter route | | | | | |
|  | 0.00M | no Jupiter route | | | | | |
