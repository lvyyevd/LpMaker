# 协议来源

实现时核对于 2026-09-17。代码读取的是运行时链上状态与交易所元数据，文档中的部署地址也需要启动校验。

- [Hyperliquid 信息 API](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)
- [账户模式](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/account-abstraction-modes)：2026-09-18 核对 unifiedAccount 的余额和冻结金额来自 spot state。
- [现货账户余额](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/spot)
- [永续账户与 activeAssetData](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)
- [Hyperliquid 交易 API](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint)
- [Hyperliquid WebSocket](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket)
- [订阅消息](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)
- [价格与数量精度](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size)
- [官方 Python SDK 签名实现](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/utils/signing.py)
- [签名测试向量来源](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py)：测试用固定私钥来自公开测试，不是真实账户密钥。
- [Uniswap 官方 SDK 部署地址 ROBINHOOD_ADDRESSES](https://github.com/Uniswap/sdks/blob/main/sdks/sdk-core/src/addresses.ts)
- [Uniswap V3 集中流动性](https://developers.uniswap.org/docs/get-started/concepts/liquidity-providers/concentrated-liquidity)
- [V3 NonfungiblePositionManager](https://github.com/Uniswap/v3-periphery/blob/main/contracts/NonfungiblePositionManager.sol)
- [SwapRouter02](https://github.com/Uniswap/swap-router-contracts/blob/main/contracts/SwapRouter02.sol)：继承的 V3 工厂读取方法是 `factory()`。
- [V3 池实现](https://github.com/Uniswap/v3-core/blob/main/contracts/UniswapV3Pool.sol)

当前配置：chain ID `4663`，pool `0x52e65B17fB6E5BA00Ed806f37Afcd2DaA50271Ca`，WETH `0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73`，USDG `0x5fc5360d0400a0fd4f2af552add042d716f1d168`，fee `100 / 1,000,000`。
