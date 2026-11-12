package sh.coobyk.verity.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle

@Composable
fun SettingsScreen(vm: AppViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    var url by remember(ui.baseUrl) { mutableStateOf(ui.baseUrl) }
    var token by remember(ui.token) { mutableStateOf(ui.token) }
    var showToken by remember { mutableStateOf(false) }

    BackHandler { vm.closeSettings() }

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
                "BRIDGE",
                color = DimText,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                letterSpacing = 1.5.sp,
            )
            Spacer(Modifier.height(12.dp))

            OutlinedTextField(
                value = url,
                onValueChange = { url = it },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Base URL") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                colors = fieldColors(),
            )
            Spacer(Modifier.height(12.dp))

            OutlinedTextField(
                value = token,
                onValueChange = { token = it },
                modifier = Modifier.fillMaxWidth(),
                label = { Text("API token") },
                singleLine = true,
                visualTransformation =
                    if (showToken) VisualTransformation.None
                    else PasswordVisualTransformation(),
                trailingIcon = {
                    TextButton(onClick = { showToken = !showToken }) {
                        Text(if (showToken) "Hide" else "Show", color = DimText)
                    }
                },
                colors = fieldColors(),
            )
            Spacer(Modifier.height(10.dp))

            Text(
                "URL and bearer token of `verity serve` on your LAN — " +
                    "see api_bind / api_token in config.toml on the desktop.",
                color = DimText,
                fontSize = 13.sp,
                lineHeight = 18.sp,
            )
            Spacer(Modifier.height(24.dp))

            Button(
                onClick = { vm.saveSettings(url, token) },
                enabled = url.isNotBlank(),
                shape = RoundedCornerShape(24.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = MainText,
                    contentColor = VerityBlack,
                ),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text("Save", fontWeight = FontWeight.SemiBold)
            }
        }
    }
}

@Composable
private fun fieldColors() = OutlinedTextFieldDefaults.colors(
    focusedTextColor = MainText,
    unfocusedTextColor = MainText,
    cursorColor = MainText,
    focusedBorderColor = DimText,
    unfocusedBorderColor = Hairline,
    focusedLabelColor = DimText,
    unfocusedLabelColor = DimText,
)
