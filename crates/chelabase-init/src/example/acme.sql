-- acme:schema
-- The Acme organisation's database, applied once by `chelabase-init example` as the database's
-- owner. Every statement is safe to repeat. The roles `engineering`, `finance`, `sales` and
-- `support` (the teams' Postgres roles) and the app users (public.users) exist before it runs.

-- Schemas, each owned by its team's role.
create schema if not exists engineering authorization engineering;
create schema if not exists finance authorization finance;
create schema if not exists sales authorization sales;
create schema if not exists support authorization support;

-- Customers are the app users: a row per signed-up customer, with the same id. Deleting an app
-- user deletes their public.users row, so everything that hangs off a customer cascades.
create table if not exists public.customers (
  id uuid primary key references public.users (id) on delete cascade,
  name text not null,
  email text not null unique,
  region text not null
);

-- The company directory. LEFT WITHOUT ROW-LEVEL SECURITY ON PURPOSE: it is the one table
-- Studio's security view flags as reachable without RLS.
create table if not exists public.employees (
  id bigint generated always as identity primary key,
  email text not null unique,
  name text not null,
  department text not null,
  level text not null check (level in ('junior', 'mid', 'senior', 'lead', 'executive'))
);

create table if not exists sales.orders (
  id bigint generated always as identity primary key,
  customer_id uuid not null references public.customers (id) on delete cascade,
  total numeric(10, 2) not null check (total >= 0),
  status text not null default 'open' check (status in ('open', 'paid', 'shipped')),
  created_at timestamptz not null default now()
);

create table if not exists finance.invoices (
  id bigint generated always as identity primary key,
  order_id bigint not null references sales.orders (id) on delete cascade,
  customer_id uuid not null references public.customers (id) on delete cascade,
  amount numeric(10, 2) not null,
  issued_on date not null,
  paid boolean not null default false
);

create table if not exists support.tickets (
  id bigint generated always as identity primary key,
  customer_id uuid not null references public.customers (id) on delete cascade,
  subject text not null,
  region text not null,
  status text not null default 'open' check (status in ('open', 'pending', 'closed')),
  opened_at timestamptz not null default now()
);

-- Row-level security everywhere but public.employees.
alter table public.customers enable row level security;
alter table sales.orders enable row level security;
alter table finance.invoices enable row level security;
alter table support.tickets enable row level security;

drop policy if exists customers_own on public.customers;
create policy customers_own on public.customers for select to authenticated
  using (id = chelabase.user_id());
-- Staff read every customer (Sales takes orders for them).
drop policy if exists customers_staff on public.customers;
create policy customers_staff on public.customers for select to sales, finance, support
  using (true);

-- A signed-in customer sees only their own orders and tickets; the data API runs them as
-- `authenticated` with their id in the claims.
drop policy if exists orders_own on sales.orders;
create policy orders_own on sales.orders for select to authenticated
  using (customer_id = chelabase.user_id());
drop policy if exists orders_sales on sales.orders;
create policy orders_sales on sales.orders for all to sales using (true) with check (true);
drop policy if exists orders_finance on sales.orders;
create policy orders_finance on sales.orders for select to finance using (true);

drop policy if exists invoices_finance on finance.invoices;
create policy invoices_finance on finance.invoices for all to finance using (true) with check (true);

drop policy if exists tickets_own on support.tickets;
create policy tickets_own on support.tickets for select to authenticated
  using (customer_id = chelabase.user_id());
-- Support sees the tickets of its region.
drop policy if exists tickets_support_region on support.tickets;
create policy tickets_support_region on support.tickets for all to support
  using (region = 'emea') with check (region = 'emea');

-- Privileges. Each team works in its own schema; the data API's `authenticated` role reads
-- what its policies let a customer see.
grant usage on schema sales, support to authenticated;
grant select on sales.orders, support.tickets, public.customers to authenticated;

grant usage on schema sales to finance;
grant select on sales.orders to finance;
grant usage on schema sales, public to sales;
grant select on public.customers to sales, finance, support;
grant select, insert, update, delete on all tables in schema sales to sales;
grant usage on all sequences in schema sales to sales;
grant select, insert, update, delete on all tables in schema finance to finance;
grant usage on all sequences in schema finance to finance;
grant select, insert, update, delete on all tables in schema support to support;
grant usage on all sequences in schema support to support;
grant select on public.employees to engineering, finance, sales, support;

-- The data API's default privileges in `public` gave its roles every table there: `anon` can
-- select, `authenticated` can also insert, update and delete. Customers are read-only through
-- it, and so is the directory: without RLS a write grant would let any signed-in customer
-- change it. Both keep their select (on the directory, the public key reads every row).
revoke insert, update, delete, truncate on public.customers from anon, authenticated;
revoke insert, update, delete, truncate on public.employees from anon, authenticated;

-- Seed rows. Customers come from the app users already made.
insert into public.customers (id, name, email, region)
select u.id, v.name, u.email, v.region
from public.users u
join (values
  ('carol@customer.example', 'Carol Danvers', 'emea'),
  ('dave@customer.example', 'Dave Bowman', 'amer'),
  ('erin@customer.example', 'Erin Brockovich', 'apac')
) as v (email, name, region) on lower(u.email) = v.email
on conflict (id) do nothing;

insert into public.employees (email, name, department, level) values
  ('ada@acme.example', 'Ada Lovelace', 'Engineering', 'executive'),
  ('grace@acme.example', 'Grace Hopper', 'Engineering', 'lead'),
  ('katherine@acme.example', 'Katherine Johnson', 'Engineering', 'senior'),
  ('linus@acme.example', 'Linus Torvalds', 'Engineering', 'senior'),
  ('barbara@acme.example', 'Barbara Liskov', 'Engineering', 'lead'),
  ('dennis@acme.example', 'Dennis Ritchie', 'Engineering', 'senior'),
  ('ken@acme.example', 'Ken Thompson', 'Engineering', 'senior'),
  ('margaret@acme.example', 'Margaret Hamilton', 'Engineering', 'lead'),
  ('radia@acme.example', 'Radia Perlman', 'Engineering', 'mid'),
  ('tim@acme.example', 'Tim Berners-Lee', 'Engineering', 'mid'),
  ('frances@acme.example', 'Frances Allen', 'Engineering', 'junior'),
  ('rosalind@acme.example', 'Rosalind Franklin', 'Finance', 'lead'),
  ('emmy@acme.example', 'Emmy Noether', 'Finance', 'senior'),
  ('joan@acme.example', 'Joan Clarke', 'Finance', 'senior'),
  ('luca@acme.example', 'Luca Pacioli', 'Finance', 'mid'),
  ('blaise@acme.example', 'Blaise Pascal', 'Finance', 'mid'),
  ('sofia@acme.example', 'Sofia Kovalevskaya', 'Finance', 'junior'),
  ('carl@acme.example', 'Carl Gauss', 'Finance', 'senior'),
  ('bea@acme.example', 'Beatrice Tinsley', 'Sales', 'senior'),
  ('cecilia@acme.example', 'Cecilia Payne', 'Sales', 'mid'),
  ('dorothy@acme.example', 'Dorothy Hodgkin', 'Sales', 'lead'),
  ('elizabeth@acme.example', 'Elizabeth Blackwell', 'Sales', 'mid'),
  ('florence@acme.example', 'Florence Nightingale', 'Sales', 'mid'),
  ('gertrude@acme.example', 'Gertrude Elion', 'Sales', 'junior'),
  ('hypatia@acme.example', 'Hypatia Alexandria', 'Sales', 'junior'),
  ('irene@acme.example', 'Irene Joliot-Curie', 'Sales', 'senior'),
  ('jocelyn@acme.example', 'Jocelyn Bell', 'Sales', 'mid'),
  ('lise@acme.example', 'Lise Meitner', 'Support', 'lead'),
  ('maria@acme.example', 'Maria Mitchell', 'Support', 'senior'),
  ('nettie@acme.example', 'Nettie Stevens', 'Support', 'mid'),
  ('ynes@acme.example', 'Ynes Mexia', 'Support', 'mid'),
  ('valentina@acme.example', 'Valentina Tereshkova', 'Support', 'mid'),
  ('sally@acme.example', 'Sally Ride', 'Support', 'junior'),
  ('mae@acme.example', 'Mae Jemison', 'Support', 'junior'),
  ('annie@acme.example', 'Annie Cannon', 'Support', 'senior'),
  ('henrietta@acme.example', 'Henrietta Leavitt', 'Support', 'junior'),
  ('hedy@acme.example', 'Hedy Lamarr', 'Leadership', 'executive'),
  ('alan@acme.example', 'Alan Turing', 'Leadership', 'executive'),
  ('marie@acme.example', 'Marie Curie', 'Leadership', 'executive'),
  ('rachel@acme.example', 'Rachel Carson', 'Leadership', 'executive')
on conflict (email) do nothing;

insert into sales.orders (customer_id, total, status, created_at)
select c.id, v.total, v.status, v.created_at::timestamptz
from public.customers c
join (values
  ('carol@customer.example', 120.00, 'paid', '2026-01-14'),
  ('carol@customer.example', 89.50, 'paid', '2026-02-03'),
  ('carol@customer.example', 310.25, 'shipped', '2026-03-21'),
  ('dave@customer.example', 45.00, 'paid', '2026-01-30'),
  ('dave@customer.example', 560.00, 'open', '2026-03-02'),
  ('erin@customer.example', 75.80, 'paid', '2026-02-11'),
  ('erin@customer.example', 199.99, 'open', '2026-03-28')
) as v (email, total, status, created_at) on c.email = v.email
where not exists (select 1 from sales.orders);

insert into finance.invoices (order_id, customer_id, amount, issued_on, paid)
select o.id, o.customer_id, o.total, o.created_at::date, o.status <> 'open'
from sales.orders o
where not exists (select 1 from finance.invoices);

insert into support.tickets (customer_id, subject, region, status, opened_at)
select c.id, v.subject, c.region, v.status, v.opened_at::timestamptz
from public.customers c
join (values
  ('carol@customer.example', 'Where is my order?', 'open', '2026-03-22'),
  ('carol@customer.example', 'Change my delivery address', 'closed', '2026-02-05'),
  ('dave@customer.example', 'Invoice shows the wrong total', 'open', '2026-03-03'),
  ('dave@customer.example', 'Cannot reset my password', 'pending', '2026-03-05'),
  ('erin@customer.example', 'Do you ship to Perth?', 'closed', '2026-02-12'),
  ('erin@customer.example', 'Order arrived damaged', 'open', '2026-03-30')
) as v (email, subject, status, opened_at) on c.email = v.email
where not exists (select 1 from support.tickets);

-- Each team's role owns its schema's tables. An owner skips its table's policies unless RLS is
-- forced, so it is: `support` sees only EMEA tickets, even on its own table.
alter table sales.orders force row level security;
alter table finance.invoices force row level security;
alter table support.tickets force row level security;
alter table sales.orders owner to sales;
alter table finance.invoices owner to finance;
alter table support.tickets owner to support;
