# Set a frontier budget

A frontier budget is one ceiling on what external models cost, for the whole
install: every application token and every durable job together. Set a weekly
ceiling, a monthly one, or both. When either is used up, Poundlock refuses new
requests to external models until that period resets. Local models are never
affected.

It works with or without LiteLLM. LiteLLM's per-key budgets, if you use them,
still apply on top; the frontier budget is the total you actually care about.
With a ceiling set, a LiteLLM endpoint's managed keys no longer need their own
budget and reset period before external models work.

## Set it

Open **Access → Frontier budget**, enter a weekly and/or monthly ceiling in US
dollars and choose **Save budget**. The same settings are available to the
operator API:

```http
PUT /api/v1/frontier-budget
Authorization: Bearer ADMIN_TOKEN
Content-Type: application/json

{"weekly_usd": 25, "monthly_usd": 100, "warn_at": 0.8, "utc_offset_minutes": -300}
```

- **Periods.** Weeks begin on Monday and months on the 1st, at midnight at
  `utc_offset_minutes` (minutes east of UTC; `-300` is UTC−5). The offset is
  fixed: it does not follow daylight saving.
- **Warning.** At `warn_at` of a ceiling (80% by default) clients are told the
  budget is running low.
- **Removing a ceiling.** Save without it. With no ceiling, external spend is
  still recorded and shown; nothing is refused.

`GET /api/v1/frontier-budget` returns the settings, each period's standing, and
this period's spend by application and model.

## How spend is measured

Poundlock meters every request to a model declared external: from an editor,
Dispatch, a durable job, or the operator's own **Test inference** and **Verify
reasoning**. For each request it records the cost from the most reliable source
available:

1. the gateway's own figure (`x-litellm-response-cost`);
2. the provider's figure in the response (`usage.cost`, as OpenRouter reports
   it);
3. the tokens the response reports, at the model's published price;
4. otherwise, the request's admission estimate, which assumes the whole output
   budget was used.

Streamed requests are asked to end with their usage (`stream_options:
include_usage`), so step 4 is rare for clients. **Test inference** always
takes it, because the check keeps no usage. It counts high on purpose: a ceiling may
stop work slightly early, never late. A request that the upstream refuses, or
that never reaches it, costs nothing.

Every figure is an estimate, never a bill. Compare against your provider's
billing when it matters.

**An external model needs a price to run under a ceiling.** Without one
Poundlock cannot meter it, so it is refused with `frontier_budget_unpriced`.
Gateway routes normally publish one; for a direct provider endpoint, choose
**Set price** on the model in **Models** and enter its input and output price
per million tokens. Models without a price say so there.

## When a ceiling is reached

Poundlock checks each external request before sending it. A request is admitted
while the period's recorded spend, plus the estimates of requests still
running, is below the ceiling. Requests already running finish, so a period can
end slightly over its ceiling, by at most what was in flight.

After that, external requests get HTTP 403 with code
`frontier_budget_exhausted`, naming the period and when it resets. Nothing is
sent upstream. Durable jobs that use an external model fail their next model
request the same way. Raise the ceiling, or wait for the reset, to continue.

## What clients see

- `GET /api/v1/usage` includes `frontier_budget`: the install's standing for
  each period. Every application sees the same totals, never who spent them.
- On `/v1/models` and access preflight, an external model is `blocked` with
  `frontier_budget_exhausted` or `frontier_budget_unpriced`, or warned with
  `frontier_budget_low`, and carries a short `frontier_budget` summary.
- Responses from external models carry `x-poundlock-frontier-budget-state`
  and `x-poundlock-frontier-budget-remaining`, measured before that request.

## What is stored

Each metered request leaves one row: when, which application token and
model, token counts, the cost and where the cost came from. No prompt or
response text. Rows older than about 13 months are removed.
