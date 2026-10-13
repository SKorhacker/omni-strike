import type { NextConfig } from "next";

// The browser only talks to this Next server; /api/* is proxied to the Rust backend
// (loopback by default), so the backend never needs CORS.
const backend = process.env.OA_BACKEND_URL ?? "http://127.0.0.1:8080";

// Same headers as the Caddy front, so a deployment without Caddy is not left bare.
// Next.js inlines its bootstrap scripts, hence 'unsafe-inline'; wallet icons are data: URIs.
const securityHeaders = [
  { key: "Content-Security-Policy", value: "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'" },
  { key: "X-Content-Type-Options", value: "nosniff" },
  { key: "X-Frame-Options", value: "DENY" },
  { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
  { key: "Permissions-Policy", value: "camera=(), microphone=(), geolocation=(), payment=()" },
  { key: "Cross-Origin-Opener-Policy", value: "same-origin" },
];

const nextConfig: NextConfig = {
  poweredByHeader: false,
  // Production only: the dev server's hot reload needs eval, which this CSP forbids.
  async headers() {
    return process.env.NODE_ENV === "production" ? [{ source: "/:path*", headers: securityHeaders }] : [];
  },
  async rewrites() {
    return [
      { source: "/api/health/:path*", destination: `${backend}/health/:path*` },
      { source: "/api/v1/:path*", destination: `${backend}/v1/:path*` },
    ];
  },
};

export default nextConfig;
