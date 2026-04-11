# Spread-Based Edge Detection Strategy

## The Core Insight

Your 1.7-second edge means **Binance price updates 1.7 seconds before Chainlink/Polymarket**. The key insight is:

**The Binance-Chainlink spread tells you how much edge remains.**

- **Large spread ($20-40)** = Strong edge, Chainlink hasn't caught up yet → ENTER
- **Small spread ($0-5)** = No edge, prices synchronized → DON'T ENTER  
- **Spread closing** = Chainlink catching up → EXIT

## How It Works

### Entry Logic

1. **Calculate spread**: `spread = Binance - Chainlink`
2. **Check spread size**: Only enter if `|spread| > MIN_EDGE_SPREAD` (default $20)
3. **Check spread direction**:
   - If `Binance > Chainlink` (+spread) → Buy **UP** (Chainlink will rise)
   - If `Binance < Chainlink` (-spread) → Buy **DOWN** (Chainlink will fall)
4. **Store entry values**: Save `entry_spread`, `entry_binance`, `entry_chainlink`

### Exit Logic

1. **Calculate current spread**: `current_spread = Binance - Chainlink`
2. **Check spread closure**: Exit if `|current_spread| < |entry_spread| * 0.3` (70% closed)
3. **Monitor spread velocity**: Exit faster if Chainlink catching up rapidly (>$10/sec)
4. **Fallback exits**: Still use trailing stop, trend reversal, spike fade as backup

## Example Scenarios

### Scenario 1: Strong Edge (Enter)
```
Entry:
- Binance: $72,450
- Chainlink: $72,420
- Spread: +$30 (Binance ahead)
- Direction: UP (Chainlink will rise to catch Binance)
- Action: BUY UP shares

Exit (70% closed):
- Binance: $72,455
- Chainlink: $72,446
- Current spread: +$9 (70% closed from $30)
- Action: SELL (Chainlink caught up)
```

### Scenario 2: No Edge (Reject)
```
- Binance: $72,430
- Chainlink: $72,428
- Spread: +$2 (too small)
- Action: REJECT "SPREAD_TOO_SMALL"
```

### Scenario 3: Wrong Direction (Reject)
```
- Binance: $72,420
- Chainlink: $72,450
- Spread: -$30 (Binance behind)
- Spike direction: UP
- Action: REJECT "SPREAD_WRONG_DIRECTION"
  (Spread says DOWN, but spike detection says UP - conflicting signals)
```

### Scenario 4: Rapid Catchup (Fast Exit)
```
Entry:
- Spread: +$35
- Time: T+0ms

T+500ms:
- Current spread: +$15
- Spread velocity: $40/sec (very fast)
- Action: EXIT "rapid_catchup" (don't wait for 70% closure)
```

## Configuration

### Aggressive (More Trades, Higher Risk)
```bash
MIN_EDGE_SPREAD=15.0      # Enter on smaller spreads
SPREAD_CLOSE_PCT=0.2      # Exit at 80% closure (hold longer)
```

### Balanced (Recommended)
```bash
MIN_EDGE_SPREAD=20.0      # Enter on $20+ spreads
SPREAD_CLOSE_PCT=0.3      # Exit at 70% closure
```

### Conservative (Fewer Trades, Lower Risk)
```bash
MIN_EDGE_SPREAD=30.0      # Only enter on large spreads
SPREAD_CLOSE_PCT=0.4      # Exit at 60% closure (exit faster)
```

## Why This Works

1. **Filters synchronized prices**: When Binance and Chainlink match, there's no edge
2. **Measures remaining edge**: Large spread = more time before Chainlink catches up
3. **Directional confirmation**: Spread direction must match spike direction
4. **Adaptive exits**: Exit when edge expires (spread closes), not on noise
5. **Velocity detection**: Exit faster when Chainlink catching up rapidly

## Dashboard Display

The dashboard now shows for each position:
- `entry_spread`: Spread when you entered (your initial edge)
- `current_spread`: Current Binance-Chainlink spread
- `spread_closed_pct`: How much of the edge has closed (0% = full edge, 100% = no edge)

For markets:
- `spread`: Current Binance-Chainlink spread
- `binance`: Current Binance price
- `chainlink`: Current Chainlink price

## Expected Behavior

### Good Trades (Win)
```
Entry: Spread=$30, Binance ahead
Hold: 1-2 seconds while Chainlink catches up
Exit: Spread=$9 (70% closed), Polymarket price moved in your favor
Result: Profit from Polymarket catching up to Binance
```

### Filtered Trades (Avoided)
```
Scenario 1: Spread=$5 → REJECTED (too small, no edge)
Scenario 2: Spread=$25 but wrong direction → REJECTED (conflicting signals)
Scenario 3: Spread=$18 → REJECTED (below $20 threshold)
```

### Fast Exits (Minimize Loss)
```
Entry: Spread=$30
T+300ms: Spread=$8 (rapid closure)
Exit: "rapid_catchup" before full loss
Result: Small loss instead of large loss
```

## Key Metrics to Monitor

1. **Entry spread distribution**: Are you entering on $20-40 spreads? (good)
2. **Spread closure at exit**: Are you exiting at 60-80% closure? (good)
3. **Rejection reasons**: Are most rejections "SPREAD_TOO_SMALL"? (good filtering)
4. **Hold time**: Are positions held 1-2 seconds? (matches your 1.7s edge)
5. **Win rate**: Should improve significantly with spread filtering

## Troubleshooting

### Too few trades
- Lower `MIN_EDGE_SPREAD` to 15.0
- Check if Binance-Chainlink spreads are actually occurring

### Too many losses
- Increase `MIN_EDGE_SPREAD` to 25.0 or 30.0
- Lower `SPREAD_CLOSE_PCT` to 0.4 (exit faster)

### Exiting too early
- Increase `SPREAD_CLOSE_PCT` to 0.4 or 0.5 (hold longer)
- Check spread velocity threshold (currently $10/sec)

### Exiting too late
- Lower `SPREAD_CLOSE_PCT` to 0.2 (exit at 80% closure)
- Lower spread velocity threshold for faster exits
