use anyhow::{anyhow, Context, Result};
use bigdecimal::BigDecimal;
use clap::Parser;
use futures::{stream::BoxStream, TryStreamExt};
use num_traits::ToPrimitive;
use sqlx::{Connection, PgConnection};
use web3::types::{TransactionReceipt, H256};

#[derive(Parser, Debug)]
struct Args {
    /// URL of the ethereum node.
    #[clap(long, env)]
    node: String,

    ///  URL for the order database.
    #[clap(long, env)]
    db: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let transport = web3::transports::Http::new(&args.node).context("create node transport")?;
    let web3 = web3::Web3::new(transport);
    let mut connection = PgConnection::connect(&args.db)
        .await
        .context("connect to database")?;

    let current_block = web3
        .eth()
        .block_number()
        .await
        .context("get current block")?
        .as_u64() as i64;
    let start_block = current_block - 100;
    println!("Showing settlements in the last 100 blocks\n");

    let settlements: Vec<SettlementRow> = settlements(start_block, current_block, &mut connection)
        .try_collect()
        .await
        .context("get settlements from db")?;
    for settlement in settlements {
        println!(
            "settlement in tx {} in block {}",
            Hex(&settlement.tx_hash),
            settlement.block_number
        );
        let hash = H256(settlement.tx_hash.try_into().map_err(|_| anyhow!(""))?);
        let (receipt, orders) = futures::join!(
            web3.eth().transaction_receipt(hash),
            orders(settlement.block_number, &mut connection).try_collect::<Vec<OrderRow>>()
        );
        let receipt = match receipt.context("transaction_receipt")? {
            Some(receipt) => receipt,
            None => {
                println!("transaction receipt not found");
                continue;
            }
        };
        let orders = orders.context("orders")?;
        if orders.iter().any(|order| order.sell_token.is_none()) {
            println!("order information not found (probably staging settlement)");
            continue;
        }
        println!();
        print_settlement(&receipt, &orders);
        println!(
            "\n--------------------------------------------------------------------------------\n"
        );
    }
    Ok(())
}

struct Hex<'a>(&'a [u8]);
impl<'a> std::fmt::Display for Hex<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x")?;
        for u in self.0 {
            write!(f, "{:02x}", u)?;
        }
        Ok(())
    }
}

#[derive(Debug, sqlx::FromRow)]
struct OrderRow {
    uid: Vec<u8>,
    sell_token: Option<Vec<u8>>,
    #[allow(dead_code)]
    signed_fee: Option<BigDecimal>,
    unsubsidized_fee: Option<BigDecimal>,
    earned_fee: Option<BigDecimal>,
    gas_amount: Option<f64>,
    gas_price: Option<f64>,
    sell_token_price: Option<f64>,
}

fn print_settlement(receipt: &TransactionReceipt, orders: &[OrderRow]) {
    let mut total_gas = 0.;
    let mut total_gas_eth = 0.;
    let mut total_earned_fee_eth = 0.;
    let mut total_unsubsidized_fee_eth = 0.;
    for order in orders {
        let uid = Hex(&order.uid);
        let sell_token = Hex(order.sell_token.as_ref().unwrap());
        let sell_token_price = order.sell_token_price.unwrap();
        let earned_fee = order.earned_fee.as_ref().unwrap().to_f64().unwrap();
        let earned_fee_eth = earned_fee / sell_token_price / 1e18;
        let unsubsidized_fee = order.unsubsidized_fee.as_ref().unwrap().to_f64().unwrap();
        let unsubsidized_fee_eth = unsubsidized_fee / sell_token_price / 1e18;
        let gas = order.gas_amount.unwrap();
        let gas_price = order.gas_price.unwrap();
        let gas_eth = gas * gas_price / 1e18;
        println!(
            "\
            order {uid}, sell_token {sell_token}, sell_token_price {sell_token_price:.1e}, \
            earned fee {earned_fee:.1e} ({earned_fee_eth:.1e} eth), \
            unsubsidized fee {unsubsidized_fee:.1e} ({unsubsidized_fee_eth:.1e} eth) \
            gas {gas:.1e} at price {gas_price:.1e} for a total of {gas_eth:.1e} eth \
            ",
        );
        total_gas += gas;
        total_gas_eth += gas_eth;
        total_earned_fee_eth += earned_fee_eth;
        total_unsubsidized_fee_eth += unsubsidized_fee_eth;
    }
    println!();
    println!("\
        expected from orders:\n\
        {total_gas:.1e} gas for {total_gas_eth:.1e} eth, \
        earning fees {total_earned_fee_eth:.1e} eth (unsubsidized {total_unsubsidized_fee_eth:.1e} eth)\n\
        ");
    let gas = receipt.gas_used.unwrap().to_f64_lossy();
    let gas_price = receipt.effective_gas_price.unwrap().to_f64_lossy();
    let gas_eth = gas * gas_price / 1e18;
    println!(
        "\
        transaction actually executed with:\n\
        {gas:.1e} gas for {gas_eth:.1e} eth (price {gas_price:.1e})\
        ",
    );
}

#[derive(sqlx::FromRow)]
struct SettlementRow {
    tx_hash: Vec<u8>,
    block_number: i64,
    #[allow(dead_code)]
    log_index: i64,
}

fn settlements(
    start_block: i64,
    end_block: i64,
    connection: &mut PgConnection,
) -> BoxStream<'_, Result<SettlementRow, sqlx::Error>> {
    sqlx::query_as(
        "
SELECT tx_hash, block_number, log_index
FROM settlements
WHERE block_number BETWEEN $1 AND $2
ORDER BY (block_number, log_index) ASC
;",
    )
    .bind(start_block)
    .bind(end_block)
    .fetch(connection)
}

// For simplicity nod handling multiple settlements in same block properly.
fn orders(
    settlement_block: i64,
    connection: &mut PgConnection,
) -> BoxStream<'_, Result<OrderRow, sqlx::Error>> {
    let query = "
SELECT
    t.uid, t.sum_fee as earned_fee,
    o.sell_token, o.fee_amount as signed_fee, o.full_fee_amount as unsubsidized_fee,
    f.gas_amount, f.gas_price, f.sell_token_price
FROM (
    SELECT order_uid as uid, SUM(fee_amount) as sum_fee
    FROM trades t
    WHERE block_number = $1
    GROUP BY uid
) AS t
LEFT OUTER JOIN orders o ON o.uid = t.uid
LEFT OUTER JOIN order_fee_parameters f ON f.order_uid = t.uid
;";
    sqlx::query_as(query)
        .bind(settlement_block)
        .fetch(connection)
}
