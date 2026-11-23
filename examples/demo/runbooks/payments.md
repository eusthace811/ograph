# payments-api runbook

payments-api, also known as pay-gw, authorizes card payments through CardRail.
It reads balances from ledger-db (ledger-pg) and authenticates callers with auth-service.
ledger-db (postgres) runs on the managed database tier; the primary database (ledger-db) is shared with orders-api.

## History
Past incidents: payments-api (INC-4120) and checkout (INC-4388). The rounding change shipped in payments-api (v2.3.1).
Some teams say orders-api is also called checkout, because it was split out of the old checkout backend.
