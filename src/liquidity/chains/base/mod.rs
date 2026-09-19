//! Base 主网 Uniswap V3 部署，来源见 docs/base.md；启动时仍须向链上核对。
pub mod fees;
pub mod pools;
use super::Chain;
pub static CHAIN: Chain = Chain {
    id: 8453,
    name: "Base",
    factory: "0x33128a8fC17869897dcE68Ed026d694621f6FDfD",
    position_manager: "0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1",
    swap_router: "0x2626664c2603336E57B271c5C0b26F421741e481",
    quoter_v2: Some("0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a"),
};
