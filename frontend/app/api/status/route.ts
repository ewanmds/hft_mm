import { NextResponse } from "next/server";

const API_URL = process.env.API_URL!;
const API_KEY = process.env.API_KEY!;

export async function GET() {
  const res = await fetch(`${API_URL}/api/status`, {
    headers: { Authorization: `Bearer ${API_KEY}` },
    cache: "no-store",
  });
  const data = await res.json();
  return NextResponse.json(data);
}
