# SAP Project Background

This is a SAP CAP + UI5 project. Key conventions:

## Tech Stack
- Backend: SAP Cloud Application Programming Model (CAP) with Node.js/TypeScript
- Frontend: SAPUI5 / Fiori Elements
- OData V4 services
- SAP HANA Cloud database

## Code Conventions
- CDS models in `db/` and `srv/` directories
- UI5 apps in `app/` directory
- Use `@sap/cds` for service definitions
- Follow SAP Fiori Design Guidelines for UI
- Use UI5 Tooling for build and development

## Testing
- Backend: `npm test` (uses Jest + cds.test)
- Frontend: `npm run test:ui5` (uses QUnit + OPA5)
- Integration: `npm run test:integration`

## Build & Deploy
- `npm run build` for production build
- `cf deploy` for Cloud Foundry deployment
- MTA-based multi-target application

## Important Notes
- Always use the UI5 MCP for API reference lookups
- Always use Fiori Tools MCP for app generation
- Check Jira for requirement context before planning
- Follow accessibility guidelines (WCAG 2.1 AA)
