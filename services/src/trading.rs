use std::collections::HashMap;
use std::sync::Arc;

use crate::historical_data::HistoricalDataService;
use crate::market_data::MarketDataService;
use crate::orders::OrderService;
use domain::domain::*;

pub trait TradingService {
    fn run(&mut self) -> Result<(), String>;
}

pub fn new(
    strategy_name: String,
    symbols: Vec<String>,
    capital: HashMap<String, i64>,
    market_data: Arc<impl MarketDataService + 'static + Send + Sync>,
    historical_data: Arc<impl HistoricalDataService + 'static + Send + Sync>,
    orders: Arc<impl OrderService + 'static + Send + Sync>,
) -> impl TradingService + 'static {
    implementation::Trading {
        strategy_name,
        symbols,
        capital,
        market_data,
        historical_data,
        orders,
        thread_handle: None,
    }
}

mod implementation {
    use super::*;
    use crate::orders::OrderService;
    use chrono::Local;
    use domain::domain::SymbolData;
    use std::collections::HashMap;

    pub struct Trading<
        M: MarketDataService + 'static + Send + Sync,
        H: HistoricalDataService + 'static + Send + Sync,
        O: OrderService + 'static + Send + Sync,
    > {
        pub strategy_name: String,
        pub symbols: Vec<String>,
        pub capital: HashMap<String, i64>,
        pub market_data: Arc<M>,
        pub historical_data: Arc<H>,
        pub orders: Arc<O>,
        pub thread_handle: Option<std::thread::JoinHandle<()>>,
    }

    impl<
            M: MarketDataService + Send + Sync,
            H: HistoricalDataService + Send + Sync,
            O: OrderService + Send + Sync,
        > TradingService for Trading<M, H, O>
    {
        fn run(&mut self) -> Result<(), String> {
            println!(
                "Running TradingService with strategy: {:?}",
                self.strategy_name
            );
            let symbol_data: HashMap<String, SymbolData> =
                load_history(&self.symbols, self.historical_data.clone());
            let orders: Arc<O> = self.orders.clone();

            match self.market_data.subscribe() {
                Ok(rx) => {
                    println!("TradingService subscribed to MarketDataService");
                    let strategy = Strategy::new(&self.strategy_name, self.symbols.clone());
                    let capital = self.capital.clone();

                    self.thread_handle = Some(std::thread::spawn(move || loop {
                        match rx.recv() {
                            Ok(quote) => {
                                let symbol_capital = capital.get(&quote.symbol).unwrap_or(&0);
                                println!("TradingService received quote:\n{:?}", quote);
                                handle_quote(
                                    &symbol_data,
                                    &quote,
                                    *symbol_capital,
                                    &strategy,
                                    orders.clone(),
                                );
                            }
                            Err(e) => {
                                eprintln!("Channel shut down: {}", e);
                            }
                        }
                    }));
                }

                Err(e) => return Err(format!("Failed to subscribe to MarketDataService: {}", e)),
            }
            Ok(())
        }
    }

    pub fn handle_quote<O: OrderService + Send + Sync>(
        symbol_data: &HashMap<String, SymbolData>,
        quote: &Quote,
        capital: i64,
        strategy: &Strategy,
        orders: Arc<O>,
    ) {
        if let Some(symbol_data) = symbol_data.get(&quote.symbol) {
            let maybe_position = orders.get_position(&quote.symbol);
            match strategy.handle(&quote, symbol_data) {
                Ok(signal) => match maybe_create_order(signal, maybe_position, quote, capital) {
                    Some(order) => match orders.create_order(order.clone()) {
                        Ok(o) => println!("Order created: {:?}", o),
                        Err(e) => eprintln!("Error creating order: {}", e),
                    },
                    None => (),
                },
                Err(e) => eprintln!("Error from strategy: {}", e),
            }
        }
    }

    pub fn maybe_create_order(
        signal: Signal,
        maybe_position: Option<Position>,
        quote: &Quote,
        capital: i64,
    ) -> Option<Order> {
        match signal {
            Signal::Buy => {
                // If position qty < target_position_qty, buy the difference up to capital
                let present_market_value = maybe_position
                    .map(|p| p.quantity as f64 * quote.ask)
                    .unwrap_or(0.0) as i64;
                let remaining_capital = capital - present_market_value;
                let shares = (remaining_capital as f64 / quote.ask) as i64;
                println!(
                    "Buy signal for {}; present_market_value: {}; remaining_capital: {}; shares to buy: {}",
                    quote.symbol, present_market_value, remaining_capital, shares
                );

                if shares > 0 {
                    Some(Order {
                        symbol: quote.symbol.clone(),
                        quantity: shares,
                        date: Local::now().naive_local().date(),
                        side: Side::Buy,
                        tradier_id: None,
                    })
                } else {
                    None
                }
            }

            Signal::Sell => {
                // If we have a position, unwind
                match maybe_position {
                    Some(p) => Some(Order {
                        symbol: quote.symbol.clone(),
                        quantity: p.quantity,
                        date: Local::now().naive_local().date(),
                        side: Side::Sell,
                        tradier_id: None,
                    }),
                    None => {
                        println!("None");
                        None
                    }
                }
            }

            Signal::None => None,
        }
    }

    pub fn load_history(
        symbols: &Vec<String>,
        historical_data_service: Arc<impl HistoricalDataService + 'static>,
    ) -> HashMap<String, SymbolData> {
        symbols
            .iter()
            .map(|symbol| -> (String, SymbolData) {
                let end = Local::now().naive_local().date();
                let start = end - chrono::Duration::days(20);
                println!("Loading history for {} from {} to {}", symbol, start, end);
                let query: Result<Vec<domain::domain::Day>, reqwest::Error> =
                    historical_data_service
                        .fetch(symbol, start, end)
                        .map(|h| h.day);
                match query {
                    Ok(history) => {
                        let sum = history.iter().map(|day| day.close).sum::<f64>();
                        let len = history.len() as f64;
                        let mean = sum / len;
                        let variance = history
                            .iter()
                            .map(|quote| (quote.close - mean).powi(2))
                            .sum::<f64>()
                            / len;
                        let std_dev = variance.sqrt();
                        let data = SymbolData {
                            symbol: symbol.clone(),
                            history,
                            mean,
                            std_dev,
                        };
                        println!("Loaded history for {}: {:?}", symbol, data);
                        (symbol.to_owned(), data)
                    }
                    Err(e) => panic!("Can't load history for {}: {}", symbol, e),
                }
            })
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
#[path = "./tests/trading_test.rs"]
mod trading_test;
