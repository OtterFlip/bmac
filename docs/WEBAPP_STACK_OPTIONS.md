# Thoughts on Greenfield Webapp Tech-Stack Selection

If you're just starting a new webapp and you haven't yet chosen your tech stack, I think it'd be worthwhile for you to watch [these](https://www.youtube.com/watch?v=GFtFIxrAjIs&t=2s) [two](https://www.youtube.com/watch?v=sQXFhh_PiG4) videos, so that you can get a sense for how capable a monolith stack can be on a even a single vCPU, and which stack choices may benefit you.  In this age of agentic programming I suggest you lean into biasing your stack choices towards those that will most-empower your AI-agent coding collaborators rather than the human developer, as the AI is likely going to be writing the majority of the code, and the human is more likely to be focused on the requirements and design.

I'm currently quite interested in this stack:  Rust + Tokio + Axum + Hyper + rustls + SQLx + Askama + HTMX, with PostgreSQL or SQLite for the DB layer.   The reasons for this stack choice are:

- **Server-side rendering keeps application state simple** - It can be hard to understand, initially, how valuable this item is, until you have some experience trying to synchronize state across both the client and the server. By having all state remain at the server, where it's the sole source-of-truth, there's no need for state synchronization, which is a constant code-burden and can become more complex the more your app-logic grows.  The effect of a single state is a drammatic decrease in complexity and that's a win that will continue to pay dividends as your project grows. Durable application state can remain in PostgreSQL or SQLite, with the Rust application acting primarily as a stateless transformation layer from database state to HTML.  Any state manintained by the clint is not application-model state but rather interaction-related state, such as the current position within a list, etc.  Strategic use of client polling updates, isolated to areas of the UI that benefit most from state refresh, can eliminate the need for persistent SSE connections between the clients and server, which can be a significant memory overhead at scale.

- **Minimal client-side state** - HTMX allows rich browser interaction using server-generated HTML fragments, while keeping most JavaScript limited to ephemeral interaction state such as dialogs, animations, selections, and other UI behaviors.

- **No separate frontend application required** - For applications that do not need a native mobile client or public API, there is no need to maintain a separate React application, JSON API contract, frontend state store, API client layer, or duplicated frontend/backend data models.

- **High performance with low overhead** - Rust provides native-code performance, excellent memory efficiency, and no garbage collector, making it well suited to vertically-scaled servers with large CPU and memory resources.

- **Strong compile-time correctness** - Rust's type system, ownership model, exhaustive matching, and `Send`/`Sync` checks catch many classes of bugs before the application ever runs.

- **Excellent fit for AI-assisted development** - Rust's strict compiler provides detailed, machine-readable feedback that coding agents can repeatedly use to correct generated code, reducing the amount of structurally-invalid code that reaches runtime.

- **Compile-time checked SQL** - SQLx can validate static SQL queries against the actual database schema and verify parameter and result types during development/build time, catching many SQL mistakes before deployment.

- **Embedded database migrations** - SQLx migrations can be compiled into the application executable, avoiding the need to separately deploy migration SQL files.

- **Efficient asynchronous I/O** - Tokio provides a mature async runtime for efficiently handling large numbers of concurrent network and database operations without requiring one OS thread per connection.

- **Mature HTTP stack** - Axum provides routing and request handling on top of Hyper, giving the application a high-performance HTTP/1.1 and HTTP/2 implementation while retaining a relatively simple programming model.

- **Native HTTPS support** - rustls provides a mature TLS implementation written in Rust, allowing the application to terminate HTTPS directly without requiring NGINX solely for TLS termination.

- **Compiled server-side templates** - Askama compiles HTML templates into the Rust application, providing fast rendering and compile-time integration between templates and Rust types.

- **Very small deployment surface** - Templates, migrations, HTTP handling, TLS, database access, and application logic can all reside within a single Rust executable, with only static assets and external configuration/secrets needing to accompany it.

- **Excellent vertical scalability** - The architecture can scale from a developer workstation to a very large multi-core server without requiring the application itself to become a distributed system.

- **Portable and infrastructure-independent** - The application primarily depends on standard Linux, HTTP, TLS, and SQL rather than proprietary cloud services or APIs, making it straightforward to develop locally and deploy to bare metal, VMs, or conventional hosting.

- **Simple operational model** - A production system can remain conceptually close to `browser -> Rust application -> database`, reducing the number of independently deployed services, runtimes, proxies, and application layers that must be monitored and maintained.
