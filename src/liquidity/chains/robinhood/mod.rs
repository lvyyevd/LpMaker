//! 原有 Robinhood 部署。保留原合约接口和交易行为，旧配置无需迁移。
pub mod pools;
use super::Chain;
pub static CHAIN: Chain = Chain {
    id: 4663,
    name: "Robinhood",
    factory: "0x1f7d7550b1b028f7571e69a784071f0205fd2efa",
    position_manager: "0x73991a25c818bf1f1128deaab1492d45638de0d3",
    swap_router: "0xcaf681a66d020601342297493863e78c959e5cb2",
    quoter_v2: None,
};
