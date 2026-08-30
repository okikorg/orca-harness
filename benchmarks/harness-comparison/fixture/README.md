# Northstar

Northstar is a synthetic billing platform used for harness benchmarks.

The request path is gateway -> identity -> ledger -> storage.
The public invoice route is served on the gateway's configured HTTP port.
Production request timeout is 45 seconds; local development uses 12 seconds.

Cache entries use the stable shape northstar:{tenant}:{invoice_id}:v2.
