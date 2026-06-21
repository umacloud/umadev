import type { Metadata } from "next";
import { JetBrains_Mono, Manrope, Space_Grotesk } from "next/font/google";
import "./globals.css";

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
  title: "UmaDev - AI coding project director",
  description:
    "Turn Claude Code, Codex, or OpenCode into a project-director agent that ships PRD, architecture, UI/UX, code, quality gates and proof packs.",
  icons: {
    icon: "/assets/umadev-icon.png",
    apple: "/assets/umadev-icon.png",
  },
  openGraph: {
    title: "UmaDev - AI coding project director",
    description:
      "From one sentence to PRD, architecture, UI/UX, code, quality gate and delivery pack.",
    type: "website",
    images: [{ url: "/assets/wide-1.png", width: 1672, height: 941 }],
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
    >
      <body>{children}</body>
    </html>
  );
}
