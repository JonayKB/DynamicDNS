const axios = require("axios");
require("dotenv").config();

const CLOUDFLARE_API_TOKEN = process.env.CLOUDFLARE_API_TOKEN;
const ZONE_ID = process.env.ZONE_ID;
const RECORD_ID = process.env.RECORD_ID;

async function getPublicIP() {
  const res = await axios.get("https://checkip.amazonaws.com");
  return res.data;
}

async function getDNSRecord() {
  const res = await axios.get(
    `https://api.cloudflare.com/client/v4/zones/${ZONE_ID}/dns_records/${RECORD_ID}`,
    {
      headers: {
        "Authorization": `Bearer ${CLOUDFLARE_API_TOKEN}`,
        "Content-Type": "application/json"
      }
    }
  );

  return res.data.result;
}

async function updateDNS(ip, record) {
  await axios.put(
    `https://api.cloudflare.com/client/v4/zones/${ZONE_ID}/dns_records/${RECORD_ID}`,
    {
      type: record.type,
      name: record.name,
      content: ip,
      ttl: 120,
      proxied: record.proxied
    },
    {
      headers: {
        "Authorization": `Bearer ${CLOUDFLARE_API_TOKEN}`,
        "Content-Type": "application/json"
      }
    }
  );
}

async function run() {
  try {
    const publicIP = await getPublicIP();
    const dnsRecord = await getDNSRecord();

    if (dnsRecord.content.trim() != publicIP.trim()) {
      console.log("⚠️  IP has changed, updating DNS…");
      await updateDNS(publicIP, dnsRecord);
      console.log("✅ ¡DNS updated!");
    }

  } catch (err) {
    console.error("❌ Error:", err.message);
  }
}

run();

