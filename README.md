# Agentic Flowstate API

A high-performance REST API server for the Agentic Flowstate ticketing system, built with pure Rust using Axum and AWS DynamoDB.

## Features

- **Pure Rust Implementation**: Zero Python dependencies for maximum performance
- **DynamoDB Integration**: Direct AWS SDK integration with connection pooling
- **RESTful Design**: Clean HTTP API following REST principles
- **Epic/Slice/Ticket Management**: Hierarchical task organization
- **Relationship Tracking**: Support for blocking/blocked-by ticket relationships

## Performance

- **Zero-warning compilation**: Clean Rust code with no compiler warnings
- **Connection pooling**: Efficient DynamoDB client reuse
- **Async/await**: Non-blocking I/O for high concurrency
- **Release build optimizations**: Compiled with `--release` for production performance

## Building

```bash
# Development build
cargo build

# Production build (optimized, zero warnings)
cargo build --release
```

## Running

```bash
# Development
cargo run

# Production
cargo run --release

# The server runs on http://localhost:8001
```

## API Endpoints

### Health Check

```
GET /health
```

Returns `OK` if the server is running.

### Epics

```
GET    /api/epics              # List all epics
POST   /api/epics              # Create a new epic
GET    /api/epics/:epic_id     # Get a specific epic
DELETE /api/epics/:epic_id     # Delete an epic
```

#### Create Epic Request Body

```json
{
  "title": "Epic Title",
  "notes": "Optional notes",
  "assignees": ["user1", "user2"]
}
```

### Slices

```
GET    /api/epics/:epic_id/slices              # List slices for an epic
POST   /api/epics/:epic_id/slices              # Create a slice (coming soon)
GET    /api/epics/:epic_id/slices/:slice_id    # Get a slice (coming soon)
DELETE /api/epics/:epic_id/slices/:slice_id    # Delete a slice (coming soon)
```

### Tickets

```
GET    /api/epics/:epic_id/tickets                           # List all tickets for an epic
GET    /api/epics/:epic_id/slices/:slice_id/tickets         # List tickets for a slice
POST   /api/epics/:epic_id/slices/:slice_id/tickets         # Create ticket (coming soon)
GET    /api/epics/:epic_id/slices/:slice_id/tickets/:id     # Get ticket (coming soon)
PATCH  /api/epics/:epic_id/slices/:slice_id/tickets/:id/status # Update status (coming soon)
DELETE /api/epics/:epic_id/slices/:slice_id/tickets/:id     # Delete ticket (coming soon)
```

### Ticket Relationships (Coming Soon)

```
POST   /api/epics/:epic_id/slices/:slice_id/tickets/:id/relationships
DELETE /api/epics/:epic_id/slices/:slice_id/tickets/:id/relationships
```

## Environment Configuration

The API uses the `telemetryops-shared` AWS profile for DynamoDB access. Ensure your AWS credentials are configured:

```bash
aws configure --profile telemetryops-shared
```

## Architecture

```
src/
├── main.rs           # Application entry point and route configuration
├── db/
│   ├── mod.rs       # Database module exports
│   └── client.rs    # DynamoDB connection pool
├── handlers/
│   ├── mod.rs       # Handler module exports
│   ├── epics.rs     # Epic CRUD operations
│   ├── slices.rs    # Slice operations
│   └── tickets.rs   # Ticket operations
├── models.rs        # Request/response data structures
└── utils/
    ├── mod.rs           # Utility module exports
    ├── db_helpers.rs    # DynamoDB helpers
    └── timestamp.rs     # Timestamp utilities
```

## Development Status

✅ **Completed**:
- Pure Rust migration (removed all Python code)
- Zero-warning compilation
- Epic CRUD operations (list, get, create, delete)
- Basic slice and ticket listing
- DynamoDB connection pooling
- CORS support for frontend integration

🚧 **In Progress**:
- Slice CRUD operations
- Ticket CRUD operations
- Ticket relationship management
- Update operations for all entities

## Testing

```bash
# Run tests
cargo test

# Test endpoints with curl
curl http://localhost:8001/health
curl http://localhost:8001/api/epics
curl -X POST http://localhost:8001/api/epics \
  -H "Content-Type: application/json" \
  -d '{"title": "New Epic", "notes": "Description"}'
```

## Related Projects

- `agentic-flowstate-mcp`: MCP server for Claude integration
- `agentic-flowstate-frontend`: Next.js web interface
- `agentic-flowstate-ticketing-system`: DynamoDB infrastructure