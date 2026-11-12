package sh.coobyk.verity.ui

import androidx.activity.compose.PredictiveBackHandler
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import sh.coobyk.verity.BuildConfig

/**
 * Account screen. The app signs into Telegram directly via TDLib, so there's
 * nothing to configure here — the API credentials are baked into BuildConfig
 * from `tdlib.apiId`/`tdlib.apiHash` at build time.
 */
@Composable
fun SettingsScreen(vm: AppViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()

    PredictiveBackHandler { progress ->
        try {
            progress.collect { vm.backProgress.value = it.progress }
            vm.closeSettings()
        } finally {
            vm.backProgress.value = -1f
        }
    }

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier
                .fillMaxWidth()
                .padding(start = 4.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = { vm.closeSettings() }) {
                Icon(
                    Icons.AutoMirrored.Filled.ArrowBack,
                    contentDescription = "Back",
                    tint = MainText,
                )
            }
            Text(
                "Settings",
                color = MainText,
                fontSize = 17.sp,
                fontWeight = FontWeight.SemiBold,
            )
        }
        HorizontalDivider(color = Hairline, thickness = 0.5.dp)

        Column(Modifier.padding(horizontal = 20.dp, vertical = 16.dp)) {
            Text(
                "TELEGRAM",
                color = DimText,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                letterSpacing = 1.5.sp,
            )
            Spacer(Modifier.height(12.dp))

            InfoRow("Account", ui.me?.let { "user #$it" } ?: "—")
            Spacer(Modifier.height(8.dp))
            InfoRow(
                "API ID",
                if (BuildConfig.TDLIB_API_ID != 0) {
                    BuildConfig.TDLIB_API_ID.toString()
                } else {
                    "not set — add tdlib.apiId to local.properties"
                },
            )
            Spacer(Modifier.height(10.dp))

            Text(
                "Signed in to Telegram directly via TDLib. The session lives " +
                    "in the TDLib database on this device — no bridge needed.",
                color = DimText,
                fontSize = 13.sp,
                lineHeight = 18.sp,
            )
            Spacer(Modifier.height(24.dp))

            Button(
                onClick = { vm.logout() },
                shape = RoundedCornerShape(24.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = MainText,
                    contentColor = VerityBlack,
                ),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text("Log out", fontWeight = FontWeight.SemiBold)
            }
        }
    }
}

@Composable
private fun InfoRow(label: String, value: String) {
    Row(Modifier.fillMaxWidth()) {
        Text(
            label,
            color = DimText,
            fontSize = 14.sp,
            modifier = Modifier.weight(0.35f),
        )
        Text(
            value,
            color = MainText,
            fontSize = 14.sp,
            modifier = Modifier.weight(0.65f),
        )
    }
}
