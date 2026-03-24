import { EC2Client, StartInstancesCommand } from "@aws-sdk/client-ec2";
import { NextResponse } from "next/server";

const ec2 = new EC2Client({
  region: process.env.AWS_REGION!,
  credentials: {
    accessKeyId: process.env.MY_AWS_ACCESS_KEY_ID!,
    secretAccessKey: process.env.MY_AWS_SECRET_ACCESS_KEY!,
  },
});

export async function POST() {
  await ec2.send(new StartInstancesCommand({
    InstanceIds: [process.env.EC2_INSTANCE_ID!],
  }));
  return NextResponse.json({ started: true });
}
