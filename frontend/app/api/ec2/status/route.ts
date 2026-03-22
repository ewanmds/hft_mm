import { EC2Client, DescribeInstancesCommand } from "@aws-sdk/client-ec2";
import { NextResponse } from "next/server";

const ec2 = new EC2Client({
  region: process.env.AWS_REGION!,
  credentials: {
    accessKeyId: process.env.AWS_ACCESS_KEY_ID!,
    secretAccessKey: process.env.AWS_SECRET_ACCESS_KEY!,
  },
});

export async function GET() {
  const res = await ec2.send(new DescribeInstancesCommand({
    InstanceIds: [process.env.EC2_INSTANCE_ID!],
  }));
  const state = res.Reservations?.[0]?.Instances?.[0]?.State?.Name ?? "unknown";
  return NextResponse.json({ state });
}
