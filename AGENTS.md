# LpMaker development

- Keep strategy calculations independent of RPC, signing and platform ABI.
- Default to paper mode; implementing a live path does not authorize running it with real funds.
- Never read or print user private keys while debugging. Signing tests use public, fixed test vectors.
- Unknown exchange results and unconfirmed EVM transactions must remain persisted and block duplicate sends.
- Do not remove the downtrend/volatility pause or turn cooldown expiry into automatic LP rebuilding.
- Add tests for changed financial logic and recovery behavior. Run cargo fmt, cargo test and cargo clippy.
- Document simulation assumptions; never fabricate LP fees or claim a backtest proves profitability.
