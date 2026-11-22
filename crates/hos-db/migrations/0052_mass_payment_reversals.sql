-- 0052: refunds and chargebacks of a mass payment reach the invoices it paid.
--
-- A combined invoice's payment became client credit that `mass_payment_items`
-- applied to each selected invoice. When that payment is refunded or charged
-- back, the credit applications are undone member by member (newest first)
-- up to the amount taken back: the member invoice gets the credit back
-- (refund: marked refunded; chargeback: reopened, services handled by the
-- chargeback policy) and `reversed` records how much of `applied` was
-- undone, so a second refund or chargeback of the same money never undoes
-- it twice. The combined invoice itself is closed (refunded), not reopened:
-- the debt lives on the member invoices.

alter table mass_payment_items
    add column reversed numeric(20, 6) not null default 0 check (reversed >= 0);
