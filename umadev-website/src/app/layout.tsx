import type { Metadata } from "next";
import { JetBrains_Mono, Manrope, Space_Grotesk } from "next/font/google";
import "./globals.css";
import { asset } from "./content";

const manrope = Manrope({
  variable: "--font-manrope",
  subsets: ["latin"],
  display: "swap",
});

const spaceGrotesk = Space_Grotesk({
  variable: "--font-space",
  subsets: ["latin"],
  display: "swap",
});

const jetBrainsMono = JetBrains_Mono({
  variable: "--font-mono",
  subsets: ["latin"],
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL(process.env.NEXT_PUBLIC_SITE_URL ?? "http://localhost:3000"),
  title: "UmaDev - a whole AI development team",
  description:
    "UmaDev is a whole AI development team — product manager, architect, designer, frontend, backend, QA, security, DevOps — that borrows your logged-in Claude Code / Codex / OpenCode brain to turn one idea into a shippable, commercial-grade app.",
  icons: {
    icon: asset("/assets/umadev-icon.png"),
    apple: asset("/assets/umadev-icon.png"),
  },
  openGraph: {
    title: "UmaDev - a whole AI development team",
    description:
      "Eight specialists collaborating like a real team to turn your idea into a shippable, commercial-grade app — PRD, API contract, design system, build, tests, security audit and a delivery proof pack.",
    type: "website",
    images: [{ url: asset("/assets/wide-1.png"), width: 1672, height: 941 }],
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html
      lang="zh-CN"
      className={`${manrope.variable} ${spaceGrotesk.variable} ${jetBrainsMono.variable}`}
      suppressHydrationWarning
    >
      <body suppressHydrationWarning>{children}</body>
    </html>
  );
}
