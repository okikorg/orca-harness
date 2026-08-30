export const routes = [
  { method: "GET", path: "/health", service: "gateway" },
  { method: "GET", path: "/v2/invoices/:id", service: "gateway" },
  { method: "POST", path: "/v2/invoices", service: "ledger" },
  { method: "POST", path: "/v2/receipts", service: "ledger" },
] as const;
