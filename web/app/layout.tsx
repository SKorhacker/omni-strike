import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Omni Strike — crypto options best execution",
  description:
    "Compare crypto option prices across venues, build a route and execute it with your own wallet.",
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
