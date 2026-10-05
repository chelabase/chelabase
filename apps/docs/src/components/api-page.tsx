'use client';
import { createOpenAPIPage } from 'fumadocs-openapi/ui';

// Reference only: no playground (the API needs a bearer token; the docs site is static).
export const OpenAPIPage = createOpenAPIPage({ playground: { enabled: false } });
